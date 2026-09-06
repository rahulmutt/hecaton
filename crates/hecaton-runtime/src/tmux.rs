//! `AgentRunner` over a dedicated tmux server (Phase 2 spec §4.3).
//!
//! `ensure_agent` diverges from the spec's §4.3 table, which shows a window
//! created (or respawned) directly with the agent's script: instead it
//! always brings the window to an idle, live pane first (creating it idle if
//! absent, or `respawn-window`-ing it into idle if its pane already exited)
//! and only then attaches `pipe-pane` and `respawn-window`s into the real
//! script. tmux reads (and, once read, discards) a pane's pty output as soon
//! as the child produces it, so a script that writes immediately can race
//! ahead of a `pipe-pane` attached to that same window after it was started
//! with the real command, losing that output to the log forever; going
//! through an idle pane first, and only ever attaching `pipe-pane` to a live
//! one, removes the race (a dead pane refuses `pipe-pane` outright).

use std::collections::BTreeMap;
use std::path::PathBuf;

use hecaton_core::{
    AgentId, AgentName, AgentRunner, CrewName, CrewRef, FleetName, LaunchPlan, ObservedState,
    ProcessState, RunnerError,
};

use crate::tools::Cmd;

/// The window that keeps a session alive when every agent window is gone.
pub const ANCHOR_WINDOW: &str = "hecaton";
const WINDOW_FORMAT: &str = "#{window_name}\t#{pane_dead}\t#{pane_pid}\t#{pane_dead_status}";
/// Command for a window that should sit idle: the crew anchor, and an
/// agent's window between creation and its first `respawn-window` into the
/// real script.
const IDLE_ARGV: [&str; 3] = ["/bin/sh", "-c", "while :; do sleep 3600; done"];

pub struct TmuxRunner {
    pub tmux: PathBuf,
    pub socket: String,
}

impl TmuxRunner {
    pub fn new(tmux: PathBuf, socket: impl Into<String>) -> Self {
        Self {
            tmux,
            socket: socket.into(),
        }
    }

    fn cmd(&self) -> Cmd {
        Cmd::new(&self.tmux).args(["-L".to_string(), self.socket.clone()])
    }

    fn run(&self, id: &str, args: &[&str]) -> Result<String, RunnerError> {
        self.cmd()
            .args(args.iter().copied())
            .run()
            .map(|o| o.stdout)
            .map_err(|f| RunnerError::Tool {
                id: id.to_string(),
                subcommand: f.subcommand,
                args: f.args,
                stderr: f.stderr,
            })
    }

    /// tmux exits non-zero with "no server running" / "can't find" /
    /// "error connecting" when nothing exists; those are "absent", not
    /// errors.
    fn run_optional(&self, id: &str, args: &[&str]) -> Result<Option<String>, RunnerError> {
        match self.cmd().args(args.iter().copied()).run() {
            Ok(o) => Ok(Some(o.stdout)),
            Err(f)
                if f.stderr.contains("no server running")
                    || f.stderr.contains("can't find")
                    || f.stderr.contains("error connecting") =>
            {
                Ok(None)
            }
            Err(f) => Err(RunnerError::Tool {
                id: id.to_string(),
                subcommand: f.subcommand,
                args: f.args,
                stderr: f.stderr,
            }),
        }
    }

    fn session_target(crew: &CrewRef) -> String {
        format!("={crew}")
    }

    fn window_target(agent: &AgentId) -> String {
        format!("={}:={}", agent.crew_ref(), agent.agent)
    }

    fn windows(
        &self,
        crew: &CrewRef,
    ) -> Result<Option<BTreeMap<AgentName, ProcessState>>, RunnerError> {
        let Some(text) = self.run_optional(
            &crew.to_string(),
            &[
                "list-windows",
                "-t",
                &Self::session_target(crew),
                "-F",
                WINDOW_FORMAT,
            ],
        )?
        else {
            return Ok(None);
        };
        parse_windows(&crew.fleet, &crew.crew, &text).map(Some)
    }
}

pub(crate) fn parse_windows(
    fleet: &FleetName,
    crew: &CrewName,
    text: &str,
) -> Result<BTreeMap<AgentName, ProcessState>, RunnerError> {
    let id = format!("{fleet}/{crew}");
    let mut out = BTreeMap::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        let cols: Vec<&str> = line.split('\t').collect();
        let [name, dead, pid, status] = cols[..] else {
            return Err(RunnerError::Parse {
                id,
                message: format!("expected 4 columns in {line:?}"),
            });
        };
        if name == ANCHOR_WINDOW {
            continue;
        }
        let Ok(agent) = name.parse::<AgentName>() else {
            continue; // a window we did not create; ignore
        };
        let state = if dead == "1" {
            ProcessState::Exited {
                code: status.parse().ok(),
            }
        } else {
            ProcessState::Running {
                pid: pid.parse().map_err(|_| RunnerError::Parse {
                    id: id.clone(),
                    message: format!("bad pid in {line:?}"),
                })?,
            }
        };
        out.insert(agent, state);
    }
    Ok(out)
}

impl AgentRunner for TmuxRunner {
    fn ensure_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        if self
            .run_optional(
                &crew.to_string(),
                &["has-session", "-t", &Self::session_target(crew)],
            )?
            .is_some()
        {
            return Ok(());
        }
        self.run(
            &crew.to_string(),
            &[
                "new-session",
                "-d",
                "-s",
                &crew.to_string(),
                "-n",
                ANCHOR_WINDOW,
                "--",
                IDLE_ARGV[0],
                IDLE_ARGV[1],
                IDLE_ARGV[2],
            ],
        )
        .map(|_| ())
    }

    fn ensure_agent(&self, agent: &AgentId, plan: &LaunchPlan) -> Result<(), RunnerError> {
        let id = agent.to_string();
        let crew = agent.crew_ref();
        let state = self
            .windows(&crew)?
            .and_then(|w| w.get(&agent.agent).copied());
        let cwd = plan.cwd.display().to_string();
        let script = plan.script.display().to_string();
        let target = Self::window_target(agent);
        match state {
            None => {
                // Window absent: create it idle, not with the real script
                // directly. See the module doc for why (a `pipe-pane`
                // output-loss race).
                self.run(
                    &id,
                    &[
                        "new-window",
                        "-d",
                        "-t",
                        &Self::session_target(&crew),
                        "-n",
                        agent.agent.as_str(),
                        "-c",
                        &cwd,
                        "--",
                        IDLE_ARGV[0],
                        IDLE_ARGV[1],
                        IDLE_ARGV[2],
                    ],
                )?;
            }
            Some(ProcessState::Exited { .. }) => {
                // Window exists but its pane already exited: tmux's
                // `pipe-pane` refuses a dead pane ("target pane has
                // exited"), so revive it into the idle placeholder first,
                // for the same reason as the `None` arm above.
                self.run(
                    &id,
                    &[
                        "respawn-window",
                        "-k",
                        "-t",
                        &target,
                        "-c",
                        &cwd,
                        IDLE_ARGV[0],
                        IDLE_ARGV[1],
                        IDLE_ARGV[2],
                    ],
                )?;
            }
            Some(ProcessState::Running { .. }) => {}
        }
        // `set-option` is idempotent and, by this point, always aimed at a
        // live pane, so re-applying it here unconditionally (not only right
        // after creating or reviving the window above) repairs a window
        // that exists without it: one left by a create that failed between
        // here and `respawn-window` below, one from before this repair
        // existed, or one an operator created by hand. Without this, such a
        // window would never gain remain-on-exit, and the reconciler would
        // recreate/restart it forever.
        self.run(
            &id,
            &["set-option", "-w", "-t", &target, "remain-on-exit", "on"],
        )?;
        let log = plan
            .script
            .parent()
            .map(|p| p.join("logs").join("tmux.log"))
            .unwrap_or_else(|| PathBuf::from("tmux.log"));
        // No `-o` here: tmux tracks "already piping" on the pane itself, and
        // that flag survives both the pane dying and `respawn-window`
        // reviving it with a fresh pty. `-o` reads that stale flag, believes
        // a pipe is already attached, and silently drops the (already-dead)
        // one without opening a new one — the revived pane then never gets
        // piped at all. Without `-o`, `pipe-pane` always closes any existing
        // pipe and opens this one, which is exactly what every call site
        // wants: this runs immediately before `respawn-window` starts the
        // process whose output we actually want captured, so there is never
        // a live process (from *this* pane, going forward) whose output
        // could fall in the close/reopen gap.
        self.run(
            &id,
            &[
                "pipe-pane",
                "-t",
                &target,
                &format!(
                    "cat >> {}",
                    crate::quote::sh_quote(&log.display().to_string())
                ),
            ],
        )?;
        self.run(
            &id,
            &["respawn-window", "-k", "-t", &target, "-c", &cwd, &script],
        )
        .map(|_| ())
    }

    fn stop_agent(&self, agent: &AgentId) -> Result<(), RunnerError> {
        self.run_optional(
            &agent.to_string(),
            &["kill-window", "-t", &Self::window_target(agent)],
        )
        .map(|_| ())
    }

    fn stop_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        self.run_optional(
            &crew.to_string(),
            &["kill-session", "-t", &Self::session_target(crew)],
        )
        .map(|_| ())
    }

    fn observe(&self, fleet: &FleetName) -> Result<ObservedState, RunnerError> {
        let mut out = ObservedState::default();
        let Some(sessions) =
            self.run_optional(fleet.as_str(), &["list-sessions", "-F", "#{session_name}"])?
        else {
            return Ok(out);
        };
        let prefix = format!("{fleet}/");
        for name in sessions.lines() {
            let Some(crew_name) = name.strip_prefix(&prefix) else {
                continue;
            };
            let Ok(crew) = crew_name.parse::<CrewName>() else {
                continue;
            };
            let crew_ref = CrewRef {
                fleet: fleet.clone(),
                crew: crew.clone(),
            };
            let windows = self.windows(&crew_ref)?.unwrap_or_default();
            out.crews.insert(crew, windows);
        }
        Ok(out)
    }

    fn send_text(&self, agent: &AgentId, text: &str, submit: bool) -> Result<(), RunnerError> {
        let id = agent.to_string();
        let target = Self::window_target(agent);
        self.run(&id, &["send-keys", "-t", &target, "-l", "--", text])?;
        if submit {
            self.run(&id, &["send-keys", "-t", &target, "Enter"])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_running_dead_and_skips_anchor_and_foreign_windows() {
        let f: FleetName = "f".parse().unwrap();
        let c: CrewName = "c".parse().unwrap();
        let text = "hecaton\t0\t10\t\nalice\t0\t42\t\nbob\t1\t43\t137\nNot_An_Agent\t0\t1\t\n";
        let w = parse_windows(&f, &c, text).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(
            w[&"alice".parse::<AgentName>().unwrap()],
            ProcessState::Running { pid: 42 }
        );
        assert_eq!(
            w[&"bob".parse::<AgentName>().unwrap()],
            ProcessState::Exited { code: Some(137) }
        );
        assert!(
            parse_windows(&f, &c, "garbage")
                .unwrap_err()
                .to_string()
                .contains("expected 4 columns")
        );
    }

    #[test]
    fn targets_use_exact_match_prefixes() {
        let a: AgentId = "f/c/a".parse().unwrap();
        assert_eq!(TmuxRunner::session_target(&a.crew_ref()), "=f/c");
        assert_eq!(TmuxRunner::window_target(&a), "=f/c:=a");
    }
}
