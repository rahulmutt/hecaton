#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use hecaton_api::{AgentSettings, CredentialBundle, CrewSpec, FleetSpec, GitAuth, GitSettings};
use hecaton_core::{Fleet, HookTarget, Keep, Materializer, ResolvedAgent};
use hecaton_runtime::{Runtime, embedded_system_tools};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        // A pre-commit hook (this crate's own, if the test suite runs from
        // one) leaks these into the environment for a linked worktree; left
        // in place they would make this fixture's `-C`-less git calls
        // operate on the real repository instead of `dir`.
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn fleet(repo_url: &str) -> Fleet {
    let mut s = AgentSettings::default();
    s.claude.binary = "/bin/sh".into();
    s.tools = BTreeMap::new();
    Fleet::try_from(FleetSpec {
        name: "f".into(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: repo_url.into(),
                git_ref: "main".into(),
                git: GitSettings {
                    push: false,
                    auth: GitAuth::None,
                    ..GitSettings::default()
                },
                agents: BTreeMap::from([("a".to_string(), s)]),
            },
        )]),
    })
    .unwrap()
}

#[test]
fn materialize_then_remove_round_trip() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("materialize");
    let layout = support::layout(&root);
    // system table: only gh, seeded from the host so mise install is offline
    let gh_version = embedded_system_tools()["gh"].clone();
    let out = Command::new(&tools.mise)
        .args(["where", &format!("gh@{gh_version}")])
        .output()
        .unwrap();
    if !support::require_or_skip("gh install to seed from", out.status.success()) {
        return;
    }
    let src = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let dst = layout
        .mise_data_dir()
        .join("installs")
        .join("gh")
        .join(&gh_version);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    assert!(
        Command::new("cp")
            .args(["-r", &src, &dst.display().to_string()])
            .status()
            .unwrap()
            .success()
    );
    std::fs::create_dir_all(&layout.config_root).unwrap();
    std::fs::write(
        layout.system_mise_toml(),
        format!("[tools]\ngh = \"{gh_version}\"\n"),
    )
    .unwrap();

    let work = root.join("up");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README"), "x").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("up.git");
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

    let f = fleet(&format!("file://{}", bare.display()));
    let rt = Runtime::new(layout.clone(), tools);
    let agent = ResolvedAgent::from_fleet(&f).remove(0);
    let crew = agent.id.crew_ref();
    let creds = CredentialBundle::default();
    let hooks = HookTarget {
        url: "https://127.0.0.1:7643".into(),
        secret: "s".into(),
    };

    rt.ensure_crew(&crew, &agent.repo, &agent.git_ref, &agent.git, &creds)
        .unwrap();
    let plan = rt.materialize(&agent, &creds, &hooks).unwrap();
    let paths = layout.agent(&agent.id);
    assert!(paths.workspace.join("README").exists());
    assert!(paths.launch.exists() && paths.profile.exists() && paths.mise_toml.exists());
    assert!(paths.claude_dir().join("settings.json").exists());
    assert!(
        !paths.claude_dir().join(".credentials.json").exists(),
        "no creds in the bundle → no file"
    );
    assert_eq!(plan.cwd, paths.workspace);
    // idempotent
    rt.ensure_crew(&crew, &agent.repo, &agent.git_ref, &agent.git, &creds)
        .unwrap();
    rt.materialize(&agent, &creds, &hooks).unwrap();

    // Exercise remove_crew's per-agent worktree-removal loop while the
    // agent's worktree still exists: removing the agent first would empty
    // `agents/` and this loop would never run.
    let crew_paths = layout.crew(&crew);
    rt.remove_crew(
        &crew,
        Keep {
            repos: true,
            sessions: false,
        },
    )
    .unwrap();
    assert!(!paths.root.exists());
    assert!(crew_paths.repo.exists());
    let worktrees = git(&crew_paths.repo, &["worktree", "list", "--porcelain"]);
    assert!(
        !worktrees
            .lines()
            .any(|l| l.strip_prefix("worktree ").map(Path::new) == Some(paths.workspace.as_path())),
        "worktree still registered: {worktrees}"
    );

    rt.remove_agent(&agent.id).unwrap(); // already gone: must be a no-op
    rt.remove_crew(&crew, Keep::default()).unwrap();
    assert!(!layout.crew(&crew).root.exists());
}

#[test]
fn gh_auth_without_a_token_is_a_clear_error() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("materialize-noauth");
    let rt = Runtime::new(support::layout(&root), tools);
    let f = fleet("file:///nowhere.git");
    let agent = ResolvedAgent::from_fleet(&f).remove(0);
    let err = rt
        .ensure_crew(
            &agent.id.crew_ref(),
            &agent.repo,
            "main",
            &GitSettings::default(),
            &CredentialBundle::default(),
        )
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "f/c: git.auth is gh but no gh token was provided (run `gh auth login` on the client)"
    );
}
