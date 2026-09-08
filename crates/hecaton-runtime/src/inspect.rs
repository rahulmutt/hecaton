//! `WorkspaceReader` over real git (Spec C §3.2): the agent's worktree
//! against the crew's base, one file, one listing. Reads only, with the
//! repository's config escape hatches closed — an agent can write the
//! shared `.git/config` and `.gitattributes`, and this code runs as the
//! daemon.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use hecaton_api::{
    EntryKind, FileDiff, FileStatus, TreeEntry, WORKSPACE_FILE_COUNT_LIMIT, WORKSPACE_FILE_LIMIT,
    WORKSPACE_PATCH_LIMIT, WorkspaceDiff, WorkspaceTree, check_path,
};
use hecaton_core::{AgentId, WorkspaceError, WorkspaceReader};

use crate::layout::CrewPaths;
use crate::materializer::Runtime;
use crate::tools::Cmd;

/// Command-line config beats every config file: whatever an agent wrote
/// into the shared `.git/config`, no program runs from it here.
const CONFIG: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.quotePath=true",
    "-c",
    "diff.noprefix=false",
];
/// On every `diff`: no external diff driver, no textconv, no colour.
const DIFF_FLAGS: &[&str] = &["--no-ext-diff", "--no-textconv", "--no-color"];

impl Runtime {
    /// The agent's worktree and crew paths; `Missing` when the worktree
    /// directory does not exist (not materialized, or purged).
    fn workspace_of(&self, agent: &AgentId) -> Result<(PathBuf, CrewPaths), WorkspaceError> {
        let paths = self.layout.agent(agent);
        if !paths.workspace.is_dir() {
            return Err(WorkspaceError::Missing(agent.to_string()));
        }
        Ok((paths.workspace, self.layout.crew(&agent.crew_ref())))
    }

    /// One git call in the worktree, logged to the crew's `git.log` like
    /// `Workspace::git`, with the same `GIT_*` scrub, no optional locks,
    /// fsmonitor off and hooks pointed at an empty directory.
    fn inspect_git(
        &self,
        id: &str,
        crew: &CrewPaths,
        workspace: &Path,
        args: &[&str],
        accepted: &[i32],
    ) -> Result<String, WorkspaceError> {
        let no_hooks = crew.root.join("no-hooks");
        let _ = std::fs::create_dir_all(&no_hooks);
        let mut cmd = Cmd::new(&self.tools.git).log(&crew.root.join("logs").join("git.log"));
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_PREFIX",
            "GIT_COMMON_DIR",
        ] {
            cmd = cmd.env_remove(var);
        }
        cmd = cmd
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(CONFIG.iter().copied())
            .args([
                "-c".to_string(),
                format!("core.hooksPath={}", no_hooks.display()),
            ])
            .args(["-C".to_string(), workspace.display().to_string()])
            .args(args.iter().copied());
        cmd.run_with_exit_codes(accepted)
            .map(|o| o.stdout)
            .map_err(|f| WorkspaceError::Tool {
                id: id.to_string(),
                subcommand: f.subcommand,
                args: f.args,
                stderr: f.stderr,
            })
    }
}

fn split_z(s: &str) -> BTreeSet<String> {
    s.split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// `diff --name-status -z --find-renames` → one `FileDiff` per record,
/// patches empty. `R`/`C` records carry two paths; a torn pair ends the
/// parse.
pub fn parse_name_status(z: &str) -> Vec<FileDiff> {
    let mut out = Vec::new();
    let mut parts = z.split('\0').filter(|p| !p.is_empty());
    while let Some(code) = parts.next() {
        let status = match code.chars().next() {
            Some('A') => FileStatus::Added,
            Some('D') => FileStatus::Deleted,
            Some('R') => FileStatus::Renamed,
            Some('C') => FileStatus::Copied,
            Some('T') => FileStatus::Typechange,
            _ => FileStatus::Modified,
        };
        let (old_path, path) = match status {
            FileStatus::Renamed | FileStatus::Copied => match (parts.next(), parts.next()) {
                (Some(old), Some(new)) => (Some(old.to_string()), new.to_string()),
                _ => break,
            },
            _ => match parts.next() {
                Some(p) => (None, p.to_string()),
                None => break,
            },
        };
        out.push(FileDiff {
            path,
            old_path,
            status,
            uncommitted: false,
            binary: false,
            patch: String::new(),
            truncated: false,
        });
    }
    out
}

/// `(patch, binary, truncated)`: a binary diff keeps no patch; a long one
/// is cut at the last line boundary under `WORKSPACE_PATCH_LIMIT`.
pub fn shape_patch(raw: String) -> (String, bool, bool) {
    if raw
        .lines()
        .any(|l| l.starts_with("Binary files ") || l.starts_with("GIT binary patch"))
    {
        return (String::new(), true, false);
    }
    if raw.len() <= WORKSPACE_PATCH_LIMIT {
        return (raw, false, false);
    }
    let end = raw[..WORKSPACE_PATCH_LIMIT]
        .rfind('\n')
        .map_or(0, |i| i + 1);
    (raw[..end].to_string(), false, true)
}

fn io_error(path: &Path, e: std::io::Error) -> WorkspaceError {
    WorkspaceError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// `symlink_metadata`: never follows the final component.
fn meta_of(path: &Path) -> Result<std::fs::Metadata, WorkspaceError> {
    std::fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            WorkspaceError::NoSuchPath
        } else {
            io_error(path, e)
        }
    })
}

/// Defence in depth against a symlinked ancestor: the canonical path must
/// still sit under the canonical worktree.
fn confine(workspace: &Path, full: &Path) -> Result<(), WorkspaceError> {
    let root = workspace
        .canonicalize()
        .map_err(|e| io_error(workspace, e))?;
    let real = full.canonicalize().map_err(|e| io_error(full, e))?;
    if real.starts_with(&root) {
        Ok(())
    } else {
        Err(WorkspaceError::InvalidPath("escapes the worktree".into()))
    }
}

impl WorkspaceReader for Runtime {
    fn diff(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceDiff, WorkspaceError> {
        let id = agent.to_string();
        let (ws, crew) = self.workspace_of(agent)?;
        let git = |args: &[&str], ok: &[i32]| self.inspect_git(&id, &crew, &ws, args, ok);
        let head = git(&["rev-parse", "HEAD"], &[0])?.trim().to_string();
        let merge_base = git(&["merge-base", base_ref, "HEAD"], &[0])?
            .trim()
            .to_string();
        let mut files = parse_name_status(&git(
            &["diff", "--name-status", "-z", "--find-renames", &merge_base],
            &[0],
        )?);
        let dirty = split_z(&git(&["diff", "--name-only", "-z", "HEAD"], &[0])?);
        let untracked = split_z(&git(
            &["ls-files", "--others", "--exclude-standard", "-z"],
            &[0],
        )?);
        for path in &untracked {
            files.push(FileDiff {
                path: path.clone(),
                old_path: None,
                status: FileStatus::Added,
                uncommitted: true,
                binary: false,
                patch: String::new(),
                truncated: false,
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let truncated = files.len() > WORKSPACE_FILE_COUNT_LIMIT;
        files.truncate(WORKSPACE_FILE_COUNT_LIMIT);
        for f in &mut files {
            f.uncommitted = f.uncommitted
                || dirty.contains(&f.path)
                || f.old_path.as_deref().is_some_and(|o| dirty.contains(o));
            let mut args: Vec<&str> = vec!["diff"];
            args.extend(DIFF_FLAGS);
            let raw = if untracked.contains(&f.path) {
                args.extend(["--no-index", "-U3", "--", "/dev/null", f.path.as_str()]);
                git(&args, &[0, 1])?
            } else {
                args.extend(["-U3", "--find-renames", merge_base.as_str(), "--"]);
                if let Some(old) = &f.old_path {
                    args.push(old);
                }
                args.push(&f.path);
                git(&args, &[0])?
            };
            let (patch, binary, cut) = shape_patch(raw);
            f.patch = patch;
            f.binary = binary;
            f.truncated = cut;
        }
        Ok(WorkspaceDiff {
            base_ref: base_ref.to_string(),
            merge_base,
            head,
            files,
            truncated,
        })
    }

    fn read_file(&self, agent: &AgentId, path: &str) -> Result<Vec<u8>, WorkspaceError> {
        check_path(path).map_err(WorkspaceError::InvalidPath)?;
        let (ws, _) = self.workspace_of(agent)?;
        let full = ws.join(path);
        let meta = meta_of(&full)?;
        if !meta.is_file() {
            return Err(WorkspaceError::NotAFile);
        }
        confine(&ws, &full)?;
        if meta.len() > WORKSPACE_FILE_LIMIT {
            return Err(WorkspaceError::TooLarge {
                limit: WORKSPACE_FILE_LIMIT,
            });
        }
        std::fs::read(&full).map_err(|e| io_error(&full, e))
    }

    fn list_dir(&self, agent: &AgentId, path: &str) -> Result<WorkspaceTree, WorkspaceError> {
        check_path(path).map_err(WorkspaceError::InvalidPath)?;
        let (ws, _) = self.workspace_of(agent)?;
        let full = if path.is_empty() {
            ws.clone()
        } else {
            ws.join(path)
        };
        if !meta_of(&full)?.is_dir() {
            return Err(WorkspaceError::NotADirectory);
        }
        confine(&ws, &full)?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&full).map_err(|e| io_error(&full, e))? {
            let entry = entry.map_err(|e| io_error(&full, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            // `DirEntry::metadata` does not follow symlinks.
            let m = entry.metadata().map_err(|e| io_error(&entry.path(), e))?;
            let kind = if m.is_file() {
                EntryKind::File
            } else if m.is_dir() {
                EntryKind::Dir
            } else if m.file_type().is_symlink() {
                EntryKind::Symlink
            } else {
                EntryKind::Other
            };
            entries.push(TreeEntry {
                name,
                kind,
                size: m.is_file().then_some(m.len()),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(WorkspaceTree {
            path: path.to_string(),
            entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hecaton_api::FileStatus;

    #[test]
    fn name_status_z_parses_statuses_and_rename_pairs() {
        let z = "M\0README\0R100\0LICENSE\0COPYING\0A\0src/lib.rs\0D\0gone\0T\0link\0C75\0a\0b\0";
        let files = parse_name_status(z);
        let got: Vec<(&str, Option<&str>, FileStatus)> = files
            .iter()
            .map(|f| (f.path.as_str(), f.old_path.as_deref(), f.status))
            .collect();
        assert_eq!(
            got,
            vec![
                ("README", None, FileStatus::Modified),
                ("COPYING", Some("LICENSE"), FileStatus::Renamed),
                ("src/lib.rs", None, FileStatus::Added),
                ("gone", None, FileStatus::Deleted),
                ("link", None, FileStatus::Typechange),
                ("b", Some("a"), FileStatus::Copied),
            ]
        );
        assert!(
            files
                .iter()
                .all(|f| !f.uncommitted && !f.binary && f.patch.is_empty())
        );
        assert!(parse_name_status("").is_empty());
        assert_eq!(
            parse_name_status("R100\0only-old\0").len(),
            0,
            "a torn pair is dropped"
        );
    }

    #[test]
    fn a_patch_is_marked_binary_or_cut_at_a_line_boundary() {
        assert_eq!(
            shape_patch("diff --git a/x b/x\nBinary files a/x and b/x differ\n".into()),
            (String::new(), true, false)
        );
        let small = "diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n".to_string();
        assert_eq!(shape_patch(small.clone()), (small, false, false));
        let line = format!("+{}\n", "y".repeat(98));
        let big = line.repeat(WORKSPACE_PATCH_LIMIT / 100 + 10);
        let (cut, binary, truncated) = shape_patch(big);
        assert!(!binary && truncated);
        assert!(cut.len() <= WORKSPACE_PATCH_LIMIT);
        assert!(cut.ends_with('\n'), "cut at a line boundary");
        assert_eq!(cut.len() % 100, 0);
    }
}
