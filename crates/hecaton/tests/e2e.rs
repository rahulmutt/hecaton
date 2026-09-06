//! The Phase 3 journey (spec §8): a real daemon over git, mise, nono and
//! tmux, with `hecaton dev fake-claude` in place of `claude`. Skips without
//! the tools or Landlock; `HECATON_REQUIRE_TOOLS=1` (CI) fails instead.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use hecaton_api::{AgentPhase, FleetPhase};
use hecaton_core::FleetRecord;

const HECATON: &str = env!("CARGO_BIN_EXE_hecaton");

fn tool(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn require_or_skip(name: &str, present: bool) -> bool {
    if present {
        return true;
    }
    if std::env::var_os("HECATON_REQUIRE_TOOLS").is_some_and(|v| v == "1") {
        panic!("{name} is required (HECATON_REQUIRE_TOOLS=1) but not available");
    }
    eprintln!("skip: {name} not available");
    false
}

/// See `hecaton-runtime/tests/support/mod.rs::landlock_works` for why the
/// probe home is a sibling of `root`.
fn landlock_works(nono: &Path, root: &Path) -> bool {
    let home = root.with_file_name(format!(
        "{}-nono-probe-home",
        root.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::create_dir_all(&home).unwrap();
    let ok = Command::new(nono)
        .args(["-s", "run", "--allow-cwd", "--", "/bin/true"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .current_dir(root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = fs::remove_dir_all(&home);
    ok
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_PREFIX")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct World {
    home: PathBuf,
    socket: String,
    tmux: PathBuf,
}

impl World {
    fn hecaton(&self) -> Command {
        let mut c = Command::new(HECATON);
        c.env("HOME", &self.home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("HECATON_API_URL")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("GH_CONFIG_DIR");
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        let out = self.hecaton().args(args).output().unwrap();
        eprintln!(
            "$ hecaton {}\n{}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "hecaton {args:?} failed");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    fn state(&self) -> PathBuf {
        self.home.join(".local").join("state").join("hecaton")
    }
    fn agent_dir(&self, agent: &str) -> PathBuf {
        self.state().join("fleets/e2e/crews/c/agents").join(agent)
    }
    fn status(&self) -> FleetRecord {
        serde_json::from_str(&self.ok(&["status", "e2e", "--json"])).unwrap()
    }
    fn window_pids(&self) -> String {
        let out = Command::new(&self.tmux)
            .args([
                "-L",
                &self.socket,
                "list-windows",
                "-t",
                "=e2e/c",
                "-F",
                "#{window_name} #{pane_pid}",
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    fn pid_of(&self, window: &str) -> String {
        self.window_pids()
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{window} ")).map(str::to_string))
            .unwrap_or_else(|| panic!("no window {window} in {:?}", self.window_pids()))
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let pid_file = self.state().join("server").join("hecaton.pid");
        if let Ok(pid) = fs::read_to_string(&pid_file) {
            let pid = pid.trim().to_string();
            let _ = Command::new("kill").args(["-TERM", &pid]).status();
            let start = Instant::now();
            while pid_file.exists() && start.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(100));
            }
            // Only escalate if the TERM wait timed out with the daemon
            // still up; a clean shutdown removes the pid file and there is
            // nothing left to kill.
            if pid_file.exists() {
                let _ = Command::new("kill").args(["-KILL", &pid]).status();
            }
        }
        let _ = Command::new(&self.tmux)
            .args(["-L", &self.socket, "kill-server"])
            .status();
    }
}

fn fleet_yaml(bare: &Path, bob_model: Option<&str>) -> String {
    let bob = match bob_model {
        Some(m) => {
            format!("      bob: {{ claude: {{ resume: true, settings: {{ model: {m} }} }} }}\n")
        }
        None => "      bob: { claude: { resume: true } }\n".to_string(),
    };
    format!(
        "apiVersion: hecaton/v1\nkind: Fleet\nname: e2e\ndefaults:\n  claude:\n    binary: \"{HECATON}\"\n    args: [dev, fake-claude, \"--verbose\"]\n    settings: {{ model: sonnet }}\n  tools: {{}}\ncrews:\n  c:\n    repo: \"file://{}\"\n    ref: main\n    git: {{ push: false, auth: none }}\n    agents:\n      alice: {{}}\n{bob}",
        bare.display()
    )
}

fn hook_secrets(w: &World) -> Vec<String> {
    let mut out = Vec::new();
    for a in ["alice", "bob"] {
        let settings =
            fs::read_to_string(w.agent_dir(a).join("home/.claude/settings.json")).unwrap();
        for piece in settings.split("Bearer ").skip(1) {
            out.push(piece.split('"').next().unwrap().to_string());
        }
    }
    assert!(!out.is_empty());
    out
}

#[test]
fn serve_up_update_down_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("hecaton-e2e-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    // empty system tool table: nothing to download, `mise exec` still resolves
    let cfg = w.home.join(".config/hecaton");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();

    // a repo with one commit and an untrusted mise.toml naming an uninstalled tool
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    fs::write(work.join("README"), "hi\n").unwrap();
    fs::write(work.join("mise.toml"), "[tools]\nnode = \"0.0.1\"\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("repo.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    let v1 = root.join("fleet.yaml");
    let v2 = root.join("fleet2.yaml");
    fs::write(&v1, fleet_yaml(&bare, None)).unwrap();
    fs::write(&v2, fleet_yaml(&bare, Some("opus"))).unwrap();

    // serve -d
    let out = w.ok(&[
        "serve",
        "-d",
        "--bind",
        "127.0.0.1:0",
        "--tmux-socket",
        &w.socket,
    ]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let url = fs::read_to_string(w.state().join("server/endpoint"))
        .unwrap()
        .trim()
        .to_string();

    // up → both Ready through the relay
    let out = w.ok(&[
        "up",
        &v1.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    let rec = w.status();
    assert_eq!(rec.status.phase, FleetPhase::Ready);
    for a in ["e2e/c/alice", "e2e/c/bob"] {
        assert_eq!(rec.status.agents[a].phase, AgentPhase::Ready, "{a}");
    }
    let alice_hash = rec.status.agents["e2e/c/alice"]
        .applied_hash
        .clone()
        .unwrap();
    let bob_hash = rec.status.agents["e2e/c/bob"].applied_hash.clone().unwrap();
    let alice_pid = w.pid_of("alice");
    // `list` pads NAME to the header's width, so the row is "e2e" + 1 pad + 2
    // separators; `status`'s headline joins with a literal two spaces.
    assert!(w.ok(&["list"]).contains("e2e   ready"));
    let argv = fs::read_to_string(w.agent_dir("alice").join("home/fake-claude.argv")).unwrap();
    assert!(
        argv.contains("--verbose") && !argv.contains("--continue"),
        "{argv}"
    );
    assert!(
        w.agent_dir("alice").join("workspace/README").exists(),
        "worktree checked out"
    );
    assert!(w.agent_dir("alice").join("home/.gitconfig").exists());

    // metrics saw the relay (SessionStart) and the HTTP hook (Notification).
    // `up` returns as soon as `Ready` is set by the SessionStart relay; the
    // fake posts `Notification` just after, so poll rather than scrape once.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let session_start = "hecaton_hook_events_total{agent=\"alice\",crew=\"c\",event=\"SessionStart\",fleet=\"e2e\"} 1";
    let notification = "hecaton_hook_events_total{agent=\"alice\",crew=\"c\",event=\"Notification\",fleet=\"e2e\"} 1";
    let start = Instant::now();
    let metrics = loop {
        let metrics = agent
            .get(format!("{url}/metrics"))
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        if metrics.contains(session_start) && metrics.contains(notification) {
            break metrics;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "metrics never showed both hooks within 10s; last scrape:\n{metrics}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(metrics.contains(session_start), "{metrics}");
    assert!(metrics.contains(notification), "{metrics}");

    // update bob only
    let out = w.ok(&[
        "update",
        &v2.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    let rec = w.status();
    assert_eq!(rec.generation, 2);
    assert_ne!(
        rec.status.agents["e2e/c/bob"]
            .applied_hash
            .as_ref()
            .unwrap(),
        &bob_hash
    );
    assert_eq!(
        rec.status.agents["e2e/c/alice"]
            .applied_hash
            .as_ref()
            .unwrap(),
        &alice_hash
    );
    assert_eq!(
        rec.status.agents["e2e/c/bob"].restarts, 0,
        "a spec change is not a crash"
    );
    assert_eq!(w.pid_of("alice"), alice_pid, "alice untouched");

    // down --keep
    let out = w.ok(&["down", "e2e", "--keep", "--timeout", "60s"]);
    assert!(out.contains("e2e  down"), "{out}");
    assert!(w.state().join("fleets/e2e/crews/c/repo").exists());
    assert!(w.agent_dir("bob").join("home").exists());
    assert!(w.state().join("fleets/e2e/fleet.json").exists());
    assert!(w.window_pids().is_empty(), "session gone");

    // up again re-applies in place; bob resumes
    let out = w.ok(&[
        "up",
        &v2.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    assert_eq!(w.status().generation, 3);
    let bob_argv = fs::read_to_string(w.agent_dir("bob").join("home/fake-claude.argv")).unwrap();
    assert!(bob_argv.contains("--continue"), "{bob_argv}");
    let alice_argv =
        fs::read_to_string(w.agent_dir("alice").join("home/fake-claude.argv")).unwrap();
    assert!(!alice_argv.contains("--continue"), "{alice_argv}");

    // no secret leaks into the daemon log or launch scripts
    let secrets = hook_secrets(&w);
    let token = fs::read_to_string(w.state().join("server/token")).unwrap();
    let log = fs::read_to_string(w.state().join("server/server.log")).unwrap();
    for s in secrets
        .iter()
        .chain(std::iter::once(&token.trim().to_string()))
    {
        assert!(!log.contains(s), "secret in server.log");
        for a in ["alice", "bob"] {
            let launch = fs::read_to_string(w.agent_dir(a).join("launch.sh")).unwrap();
            assert!(!launch.contains(s), "secret in {a}'s launch.sh");
        }
    }

    // purge
    let out = w.ok(&["down", "e2e", "--purge", "--timeout", "60s"]);
    assert!(out.contains("e2e: purged"), "{out}");
    assert!(!w.state().join("fleets/e2e").exists());
    let out = w.run(&["status", "e2e"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
    drop(w);
    assert!(
        !root
            .join("home/.local/state/hecaton/server/endpoint")
            .exists(),
        "SIGTERM cleaned up"
    );
}
