//! Tool discovery and a logged subprocess runner. Argv arrays only.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPaths {
    pub git: PathBuf,
    pub gh: PathBuf,
    pub mise: PathBuf,
    pub nono: PathBuf,
    pub tmux: PathBuf,
    /// This binary: the `SessionStart` relay hook runs it inside the
    /// sandbox (Phase 3 spec P3-3), so the profile grants it read-only.
    pub hecaton: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("required tool not found on PATH: {0}")]
pub struct MissingTool(pub String);

impl ToolPaths {
    /// Finds each tool as the first executable file on `path`. `hecaton` is
    /// this binary's own path (the `SessionStart` relay hook target), not
    /// discovered on `PATH`.
    pub fn discover_in(path: &OsStr, hecaton: &Path) -> Result<Self, MissingTool> {
        let find = |name: &str| -> Result<PathBuf, MissingTool> {
            std::env::split_paths(path)
                .map(|d| d.join(name))
                .find(|p| p.is_file())
                .ok_or_else(|| MissingTool(name.to_string()))
        };
        Ok(Self {
            git: find("git")?,
            gh: find("gh")?,
            mise: find("mise")?,
            nono: find("nono")?,
            tmux: find("tmux")?,
            hecaton: hecaton.to_path_buf(),
        })
    }
}

/// One subprocess invocation.
pub(crate) struct Cmd {
    program: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    env_removals: Vec<String>,
    cwd: Option<PathBuf>,
    log: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct CmdOutput {
    pub stdout: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CmdFailure {
    pub tool: String,
    pub subcommand: String,
    pub args: Vec<String>,
    pub stderr: String,
}

impl fmt::Display for CmdFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.tool,
            self.subcommand,
            hecaton_core::first_line(&self.stderr)
        )
    }
}

impl Cmd {
    pub(crate) fn new(program: &Path) -> Self {
        Self {
            program: program.to_path_buf(),
            args: Vec::new(),
            env: BTreeMap::new(),
            env_removals: Vec::new(),
            cwd: None,
            log: None,
        }
    }
    pub(crate) fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, a: I) -> Self {
        self.args.extend(a.into_iter().map(Into::into));
        self
    }
    pub(crate) fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }
    pub(crate) fn envs(mut self, vars: &BTreeMap<String, String>) -> Self {
        self.env
            .extend(vars.iter().map(|(k, v)| (k.clone(), v.clone())));
        self
    }
    /// Removes `k` from the child's environment, clearing any value this
    /// process inherited for it. Removals are applied in `run` after every
    /// `env`/`envs` call on this `Cmd`, so a removed key stays removed even
    /// if `env`/`envs` for it was called first.
    pub(crate) fn env_remove(mut self, k: impl Into<String>) -> Self {
        self.env_removals.push(k.into());
        self
    }
    pub(crate) fn cwd(mut self, d: &Path) -> Self {
        self.cwd = Some(d.to_path_buf());
        self
    }
    /// Append `$ argv`, stdout and stderr to this file after the run.
    pub(crate) fn log(mut self, file: &Path) -> Self {
        self.log = Some(file.to_path_buf());
        self
    }
    pub(crate) fn tool(&self) -> String {
        self.program
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
    /// First argument that does not start with `-`, e.g. `clone` in
    /// `git -C x clone`. If every argument is a flag (or this value is
    /// consumed by one, e.g. `-c` in `sh -c '…'`), falls back to the first
    /// argument, so `sh -c '…'` reports its subcommand as `-c`.
    pub(crate) fn subcommand(&self) -> String {
        let mut skip_value = false;
        for a in &self.args {
            if skip_value {
                skip_value = false;
                continue;
            }
            if a == "-C" || a == "-c" || a == "-L" || a == "-t" {
                skip_value = true;
                continue;
            }
            if !a.starts_with('-') {
                return a.clone();
            }
        }
        self.args.first().cloned().unwrap_or_default()
    }

    pub(crate) fn run(&self) -> Result<CmdOutput, CmdFailure> {
        let mut c = Command::new(&self.program);
        c.args(&self.args).envs(&self.env);
        for k in &self.env_removals {
            c.env_remove(k);
        }
        if let Some(d) = &self.cwd {
            c.current_dir(d);
        }
        let failure = |stderr: String| CmdFailure {
            tool: self.tool(),
            subcommand: self.subcommand(),
            args: self.args.clone(),
            stderr,
        };
        let out = c
            .output()
            .map_err(|e| failure(format!("cannot execute {}: {e}", self.program.display())))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if let Some(log) = &self.log {
            if let Some(dir) = log.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log) {
                let _ = writeln!(
                    f,
                    "$ {} {}\n{stdout}{stderr}[exit {}]",
                    self.tool(),
                    self.args.join(" "),
                    out.status
                );
            }
        }
        if !out.status.success() {
            return Err(failure(if stderr.trim().is_empty() {
                format!("exit status {}", out.status)
            } else {
                stderr
            }));
        }
        Ok(CmdOutput { stdout })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_finds_tools_on_path_and_names_the_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        for t in ["git", "gh", "mise", "nono"] {
            std::fs::write(dir.path().join(t), "").unwrap();
        }
        let err =
            ToolPaths::discover_in(dir.path().as_os_str(), Path::new("/opt/hecaton")).unwrap_err();
        assert_eq!(err.to_string(), "required tool not found on PATH: tmux");
        std::fs::write(dir.path().join("tmux"), "").unwrap();
        let t = ToolPaths::discover_in(dir.path().as_os_str(), Path::new("/opt/hecaton")).unwrap();
        assert_eq!(t.tmux, dir.path().join("tmux"));
        assert_eq!(t.hecaton, PathBuf::from("/opt/hecaton"));
    }

    #[test]
    fn subcommand_skips_flags_with_values() {
        let c =
            Cmd::new(Path::new("/usr/bin/git")).args(["-C", "/x", "-c", "k=v", "worktree", "add"]);
        assert_eq!(c.subcommand(), "worktree");
        assert_eq!(
            Cmd::new(Path::new("/t/tmux"))
                .args(["-L", "s", "new-window"])
                .subcommand(),
            "new-window"
        );
        assert_eq!(c.tool(), "git");
    }

    #[test]
    fn run_captures_output_logs_and_fails_on_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs").join("sh.log");
        let ok = Cmd::new(Path::new("/bin/sh"))
            .args(["-c", "echo out; echo err >&2"])
            .log(&log)
            .run()
            .unwrap();
        assert_eq!(ok.stdout, "out\n");
        let err = Cmd::new(Path::new("/bin/sh"))
            .args(["-c", "echo bad >&2; exit 3"])
            .log(&log)
            .run()
            .unwrap_err();
        assert_eq!(err.stderr, "bad\n");
        assert_eq!(err.to_string(), "sh -c: bad");
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("$ sh -c echo out"));
        assert!(logged.contains("err\n"), "stderr is logged even on success");
        assert!(logged.contains("[exit exit status: 3]"));
    }

    #[test]
    fn env_remove_clears_an_inherited_or_just_set_variable() {
        let out = Cmd::new(Path::new("/bin/sh"))
            .args(["-c", "echo \"${GIT_DIR:-unset}\""])
            .env("GIT_DIR", "/x")
            .env_remove("GIT_DIR")
            .run()
            .unwrap();
        assert_eq!(out.stdout, "unset\n");
    }

    #[test]
    fn run_reports_a_missing_program() {
        let err = Cmd::new(Path::new("/nonexistent/tool"))
            .args(["x"])
            .run()
            .unwrap_err();
        assert!(err.stderr.starts_with("cannot execute /nonexistent/tool"));
    }
}
