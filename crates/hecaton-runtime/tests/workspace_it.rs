#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::path::Path;
use std::process::Command;

use hecaton_core::RepoRef;
use hecaton_runtime::Workspace;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A bare repo with one commit on `main`, served over file://.
fn bare_repo(root: &Path) -> RepoRef {
    let work = root.join("upstream-work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README"), "hi\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("upstream.git");
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    RepoRef::parse(&format!("file://{}", bare.display())).unwrap()
}

#[test]
fn clone_worktree_reuse_and_remove() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: hecaton_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };

    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    assert!(crew.repo.join(".git").is_dir());
    assert!(!crew.repo.join("README").exists(), "--no-checkout");
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap(); // fetch path

    ws.ensure_worktree("f/c/a", &crew, &paths.workspace, "hecaton/f/c/a", "main")
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(paths.workspace.join("README")).unwrap(),
        "hi\n"
    );
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "hecaton/f/c/a"
    );
    ws.ensure_worktree("f/c/a", &crew, &paths.workspace, "hecaton/f/c/a", "main")
        .unwrap(); // idempotent

    // agent commits; remove the worktree; re-adding must keep the commit (P2-6)
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    ws.remove_worktree("f/c/a", &crew, &paths.workspace)
        .unwrap();
    assert!(!paths.workspace.exists());
    ws.ensure_worktree("f/c/a", &crew, &paths.workspace, "hecaton/f/c/a", "main")
        .unwrap();
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "HEAD"]),
        sha,
        "branch reused, commit preserved"
    );
    assert!(paths.workspace.join("work.txt").exists());

    // a second agent gets its own branch from origin/main, not from alice's
    let b = layout.agent(&"f/c/b".parse().unwrap());
    ws.ensure_worktree("f/c/b", &crew, &b.workspace, "hecaton/f/c/b", "main")
        .unwrap();
    assert!(!b.workspace.join("work.txt").exists());
    assert!(crew.root.join("logs").join("git.log").exists());
}

#[test]
fn errors_name_the_id_tool_and_first_stderr_line() {
    let Some(tools) = support::tools() else {
        return;
    };
    let root = support::temp_root("workspace-err");
    let layout = support::layout(&root);
    let crew = layout.crew(&"f/c".parse().unwrap());
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    let err = ws
        .ensure_repo(
            "f/c",
            &crew,
            &RepoRef::parse("file:///nonexistent/repo.git").unwrap(),
            "main",
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.starts_with("f/c: git clone: "), "{msg}");
    assert!(
        msg.contains("fatal") || msg.contains("does not exist") || msg.contains("not found"),
        "{msg}"
    );
}
