//! Crew clone and per-agent worktree (Phase 2 spec §4.2 step 1, P2-6).

use std::path::{Path, PathBuf};

use hecaton_core::{MaterializeError, RepoRef};

use crate::fsutil::write_atomic;
use crate::home::render_hosts_yml;
use crate::layout::CrewPaths;
use crate::tools::{Cmd, ToolPaths};

pub struct Workspace<'a> {
    pub tools: &'a ToolPaths,
    /// `GH_CONFIG_DIR` for the daemon's git calls when `git.auth: gh`.
    pub gh_config_dir: Option<PathBuf>,
}

impl Workspace<'_> {
    pub fn write_fleet_gh_config(
        dir: &Path,
        token: &str,
        id: &str,
    ) -> Result<(), MaterializeError> {
        let hosts = dir.join("hosts.yml");
        write_atomic(&hosts, render_hosts_yml(token).as_bytes(), 0o600).map_err(|e| {
            MaterializeError::Io {
                id: id.to_string(),
                path: hosts,
                message: e.to_string(),
            }
        })
    }

    fn git(&self, id: &str, crew: &CrewPaths, args: &[&str]) -> Result<String, MaterializeError> {
        let mut cmd = Cmd::new(&self.tools.git).log(&crew.root.join("logs").join("git.log"));
        if let Some(dir) = &self.gh_config_dir {
            cmd = cmd.env("GH_CONFIG_DIR", dir.display().to_string()).args([
                "-c",
                "credential.helper=",
                "-c",
                &format!(
                    "credential.helper=!{} auth git-credential",
                    self.tools.gh.display()
                ),
            ]);
        }
        cmd = cmd
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(args.iter().copied());
        cmd.run()
            .map(|o| o.stdout)
            .map_err(|f| MaterializeError::Tool {
                id: id.to_string(),
                tool: f.tool,
                subcommand: f.subcommand,
                args: f.args,
                stderr: f.stderr,
            })
    }

    /// Clone without a checkout if absent, else fetch. Idempotent.
    pub fn ensure_repo(
        &self,
        id: &str,
        crew: &CrewPaths,
        repo: &RepoRef,
        git_ref: &str,
    ) -> Result<(), MaterializeError> {
        let _ = git_ref;
        if crew.repo.join(".git").is_dir() {
            self.git(
                id,
                crew,
                &[
                    "-C",
                    &crew.repo.display().to_string(),
                    "fetch",
                    "--quiet",
                    "origin",
                ],
            )?;
        } else {
            std::fs::create_dir_all(&crew.root).map_err(|e| MaterializeError::Io {
                id: id.to_string(),
                path: crew.root.clone(),
                message: e.to_string(),
            })?;
            self.git(
                id,
                crew,
                &[
                    "clone",
                    "--quiet",
                    "--no-checkout",
                    &repo.clone_url(),
                    &crew.repo.display().to_string(),
                ],
            )?;
        }
        Ok(())
    }

    /// Reuses a registered worktree; reuses an existing branch; otherwise
    /// creates the branch from `origin/<git_ref>`.
    pub fn ensure_worktree(
        &self,
        id: &str,
        crew: &CrewPaths,
        workspace: &Path,
        branch: &str,
        git_ref: &str,
    ) -> Result<(), MaterializeError> {
        let repo = crew.repo.display().to_string();
        let list = self.git(id, crew, &["-C", &repo, "worktree", "list", "--porcelain"])?;
        let registered = list
            .lines()
            .any(|l| l.strip_prefix("worktree ").map(Path::new) == Some(workspace));
        if registered && workspace.join(".git").exists() {
            return Ok(());
        }
        self.git(id, crew, &["-C", &repo, "worktree", "prune"])?;
        let ws = workspace.display().to_string();
        let branch_exists = self
            .git(
                id,
                crew,
                &[
                    "-C",
                    &repo,
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ],
            )
            .is_ok();
        if branch_exists {
            self.git(
                id,
                crew,
                &["-C", &repo, "worktree", "add", "--quiet", &ws, branch],
            )?;
        } else {
            self.git(
                id,
                crew,
                &[
                    "-C",
                    &repo,
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    branch,
                    &ws,
                    &format!("origin/{git_ref}"),
                ],
            )?;
        }
        Ok(())
    }

    pub fn remove_worktree(
        &self,
        id: &str,
        crew: &CrewPaths,
        workspace: &Path,
    ) -> Result<(), MaterializeError> {
        if !crew.repo.join(".git").is_dir() {
            return Ok(());
        }
        let repo = crew.repo.display().to_string();
        if workspace.exists() {
            self.git(
                id,
                crew,
                &[
                    "-C",
                    &repo,
                    "worktree",
                    "remove",
                    "--force",
                    &workspace.display().to_string(),
                ],
            )?;
        }
        self.git(id, crew, &["-C", &repo, "worktree", "prune"])
            .map(|_| ())
    }
}
