#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::collections::BTreeMap;
use std::process::Command;

use hecaton_core::AgentId;
use hecaton_runtime::{Toolchain, embedded_system_tools, mise_env};

/// Copies the host's `gh@<pin>` install into the shared data dir. Returns
/// false if the host does not have it.
fn seed_gh(
    tools: &hecaton_runtime::ToolPaths,
    layout: &hecaton_runtime::StateLayout,
    version: &str,
) -> bool {
    let out = Command::new(&tools.mise)
        .args(["where", &format!("gh@{version}")])
        .output()
        .unwrap();
    if !out.status.success() {
        return false;
    }
    let src = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let dst = layout
        .mise_data_dir()
        .join("installs")
        .join("gh")
        .join(version);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    let status = Command::new("cp")
        .args(["-r", &src, &dst.display().to_string()])
        .status()
        .unwrap();
    status.success()
}

#[test]
fn installs_nothing_when_seeded_and_exec_resolves_read_only() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise+gh", false));
        return;
    };
    let root = support::temp_root("toolchain");
    let layout = support::layout(&root);
    let system = embedded_system_tools();
    let gh_version = system["gh"].clone();
    if !support::require_or_skip(
        "gh install to seed from",
        seed_gh(&tools, &layout, &gh_version),
    ) {
        return;
    }
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    std::fs::create_dir_all(&paths.root).unwrap();
    let tc = Toolchain {
        tools: &tools,
        layout: &layout,
    };
    // only gh: claude is not seeded and must not be attempted
    let only_gh: BTreeMap<String, String> =
        BTreeMap::from([("gh".to_string(), gh_version.clone())]);
    tc.write(&id, &paths, &only_gh, &BTreeMap::new(), true)
        .unwrap();
    tc.install(&id, &paths).unwrap();

    // the pool chain: does `mise exec` still resolve `gh` from the read-only
    // daemon pool via `MISE_SHARED_INSTALL_DIRS`, with the agent's own data
    // dir empty? (spec §4.4 row 2, Spec E §6)
    let ro = |on: bool| {
        let mode = if on { "a-w" } else { "u+w" };
        assert!(
            Command::new("chmod")
                .args(["-R", mode, &layout.mise_data_dir().display().to_string()])
                .status()
                .unwrap()
                .success()
        );
    };
    ro(true);
    let out = Command::new(&tools.mise)
        .args(["exec", "--", "gh", "--version"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &paths.home)
        .envs(&mise_env(&id, &paths, &layout))
        .current_dir("/")
        .output()
        .unwrap();
    ro(false);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "mise exec failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains(&gh_version), "resolved {stdout}");
    assert!(paths.logs.join("mise.toolchain.log").exists());
}
