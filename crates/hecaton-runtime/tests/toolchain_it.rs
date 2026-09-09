#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::collections::BTreeMap;
use std::process::Command;

use hecaton_core::AgentId;
use hecaton_runtime::{Toolchain, embedded_system_tools, mise_env};

/// Copies the host's `<tool>@<version>` install into `pool/installs`.
/// Returns false if the host does not have it.
fn seed_into(
    tools: &hecaton_runtime::ToolPaths,
    pool: &std::path::Path,
    tool: &str,
    version: &str,
) -> bool {
    let out = Command::new(&tools.mise)
        .args(["where", &format!("{tool}@{version}")])
        .output()
        .unwrap();
    if !out.status.success() {
        return false;
    }
    let src = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let dst = pool.join("installs").join(tool).join(version);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    Command::new("cp")
        .args(["-r", &src, &dst.display().to_string()])
        .status()
        .unwrap()
        .success()
}

/// The version this repo pins for `tool`, read from its own `mise.toml`.
fn repo_pin(tool: &str) -> String {
    let text =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../mise.toml")).unwrap();
    let doc: toml::Table = text.parse().unwrap();
    doc["tools"][tool].as_str().unwrap().to_string()
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
        seed_into(&tools, &layout.mise_data_dir(), "gh", &gh_version),
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

// The brief for this test names `jq` (fleet) and `fd` (crew) as the seeded
// tools, reasoning that both are pinned in this repo's own `mise.toml`.
// Neither actually is: `mise.toml` pins `gitleaks`, `tmux` and `gh`, and only
// those three resolve with `repo_pin` and have a host install to seed from in
// both a dev checkout and CI. `gitleaks` stands in for the fleet pool's tool,
// `tmux` for the crew pool's, and `gh` for the daemon pool's, as the brief
// already has it.
#[test]
fn each_level_installs_into_its_own_pool_and_the_agent_resolves_them_all() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise", false));
        return;
    };
    let root = support::temp_root("pools");
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew_ref = id.crew_ref();
    let fleet_paths = layout.fleet(&id.fleet);
    let crew_paths = layout.crew(&crew_ref);
    let paths = layout.agent(&id);
    std::fs::create_dir_all(&paths.root).unwrap();
    std::fs::create_dir_all(crew_paths.root.join("logs")).unwrap();

    // Seed one tool per shared level from the host so nothing downloads.
    let gitleaks = repo_pin("gitleaks");
    let tmux = repo_pin("tmux");
    let gh = repo_pin("gh");
    let seeded = seed_into(&tools, &layout.mise_data_dir(), "gh", &gh)
        && seed_into(&tools, &fleet_paths.mise_pool(), "gitleaks", &gitleaks)
        && seed_into(&tools, &crew_paths.mise_pool(), "tmux", &tmux);
    if !support::require_or_skip(
        "host installs of gh, gitleaks and tmux to seed from",
        seeded,
    ) {
        return;
    }

    let tc = Toolchain {
        tools: &tools,
        layout: &layout,
    };
    let log = crew_paths.root.join("logs").join("mise.pools.log");
    let daemon_pool = layout.mise_data_dir();
    let fleet_tools = BTreeMap::from([("gitleaks".to_string(), gitleaks.clone())]);
    // The crew also names the fleet's tool, at the fleet's own version: this
    // is what actually exercises "never re-download what an outer pool
    // holds" (Spec E §5). Declaring only the crew's own tool here, as the
    // brief's literal listing does, would make the "crew pool doesn't hold
    // the fleet's tool" assertion below true no matter what `install_level`
    // does — the crew's table would simply never have named it. `gitleaks`
    // was seeded only into the fleet pool, never the crew pool, so this only
    // passes without touching the network if the crew's `mise install`
    // actually resolves it through `MISE_SHARED_INSTALL_DIRS` instead of
    // installing its own copy.
    let crew_tools = BTreeMap::from([
        ("tmux".to_string(), tmux.clone()),
        ("gitleaks".to_string(), gitleaks.clone()),
    ]);

    let crew_id = crew_ref.to_string();
    tc.install_level(
        &crew_id,
        "fleet f",
        &fleet_paths.mise_toml,
        &fleet_paths.mise_pool(),
        std::slice::from_ref(&daemon_pool),
        &fleet_paths.installed_marker(),
        &fleet_tools,
        &log,
    )
    .unwrap();
    tc.install_level(
        &crew_id,
        "crew f/c",
        &crew_paths.mise_toml(),
        &crew_paths.mise_pool(),
        &[fleet_paths.mise_pool(), daemon_pool.clone()],
        &crew_paths.installed_marker(),
        &crew_tools,
        &log,
    )
    .unwrap();

    // The agent's table names all three; only what no pool holds is private.
    let agent_tools = BTreeMap::from([
        ("gitleaks".to_string(), gitleaks.clone()),
        ("tmux".to_string(), tmux.clone()),
        ("gh".to_string(), gh.clone()),
    ]);
    tc.write(&id, &paths, &BTreeMap::new(), &agent_tools, false)
        .unwrap();
    tc.install(&id, &paths).unwrap();

    let holds = |pool: &std::path::Path, tool: &str| pool.join("installs").join(tool).exists();
    assert!(holds(&fleet_paths.mise_pool(), "gitleaks"));
    assert!(
        !holds(&fleet_paths.mise_pool(), "tmux"),
        "crew tools stay out of the fleet pool"
    );
    assert!(holds(&crew_paths.mise_pool(), "tmux"));
    assert!(
        !holds(&crew_paths.mise_pool(), "gitleaks"),
        "the fleet pool already had it: the crew's own install must resolve \
         it through the pool chain instead of downloading a second copy"
    );
    assert!(
        !paths.mise_data_dir().join("installs").exists()
            || std::fs::read_dir(paths.mise_data_dir().join("installs"))
                .unwrap()
                .next()
                .is_none(),
        "every tool came from a pool, so nothing landed privately"
    );

    // Each tool resolves, and from the pool that owns it.
    for (tool, pool) in [
        ("gitleaks", fleet_paths.mise_pool()),
        ("tmux", crew_paths.mise_pool()),
        ("gh", daemon_pool.clone()),
    ] {
        let out = Command::new(&tools.mise)
            .args(["which", tool])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &paths.home)
            .envs(&mise_env(&id, &paths, &layout))
            .current_dir("/")
            .output()
            .unwrap();
        let resolved = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(
            out.status.success(),
            "mise which {tool}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            resolved.starts_with(&pool.display().to_string()),
            "{tool} resolved to {resolved}, expected it under {}",
            pool.display()
        );
    }

    // The markers make a second pass a no-op.
    let before = std::fs::read_to_string(fleet_paths.installed_marker()).unwrap();
    tc.install_level(
        &crew_id,
        "fleet f",
        &fleet_paths.mise_toml,
        &fleet_paths.mise_pool(),
        &[daemon_pool],
        &fleet_paths.installed_marker(),
        &fleet_tools,
        &log,
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(fleet_paths.installed_marker()).unwrap(),
        before
    );
}

#[test]
fn a_crew_pins_its_own_version_without_disturbing_the_fleets() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise", false));
        return;
    };
    let root = support::temp_root("pools-versions");
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let fleet_paths = layout.fleet(&id.fleet);
    let crew_paths = layout.crew(&id.crew_ref());
    std::fs::create_dir_all(crew_paths.root.join("logs")).unwrap();

    // Two versions of one tool: the host's pin, and a second real version
    // taken from `mise ls-remote` so nothing is invented.
    let gitleaks = repo_pin("gitleaks");
    let out = Command::new(&tools.mise)
        .args(["ls-remote", "gitleaks"])
        .output()
        .unwrap();
    let other = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .rfind(|v| !v.is_empty() && *v != gitleaks)
        .map(str::to_string);
    let Some(other) = other.filter(|_| out.status.success()) else {
        assert!(!support::require_or_skip(
            "a second gitleaks version from mise ls-remote",
            false
        ));
        return;
    };
    if !support::require_or_skip(
        "a host gitleaks install to seed from",
        seed_into(&tools, &fleet_paths.mise_pool(), "gitleaks", &gitleaks),
    ) {
        return;
    }
    // The crew's version is seeded too: this test is about placement, not
    // downloading. Reuse the same payload under the other version's name.
    let src = fleet_paths
        .mise_pool()
        .join("installs")
        .join("gitleaks")
        .join(&gitleaks);
    let dst = crew_paths
        .mise_pool()
        .join("installs")
        .join("gitleaks")
        .join(&other);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    assert!(
        Command::new("cp")
            .args(["-r", &src.display().to_string(), &dst.display().to_string()])
            .status()
            .unwrap()
            .success()
    );

    let tc = Toolchain {
        tools: &tools,
        layout: &layout,
    };
    let log = crew_paths.root.join("logs").join("mise.pools.log");
    tc.install_level(
        &id.crew_ref().to_string(),
        "crew f/c",
        &crew_paths.mise_toml(),
        &crew_paths.mise_pool(),
        &[fleet_paths.mise_pool(), layout.mise_data_dir()],
        &crew_paths.installed_marker(),
        &BTreeMap::from([("gitleaks".to_string(), other.clone())]),
        &log,
    )
    .unwrap();

    assert!(
        fleet_paths
            .mise_pool()
            .join("installs")
            .join("gitleaks")
            .join(&gitleaks)
            .exists(),
        "the fleet's version survives a crew that pins another"
    );
    // Exact contents, not just "the other version is present": the fleet's
    // version was never seeded into the crew pool and TOML cannot hold two
    // values for one key, so a negative existence check here can't fail
    // regardless of the pool-chain logic. Assert the crew pool's `gitleaks`
    // directory holds exactly one version, and it is the crew's.
    // mise also drops alias symlinks alongside the real version directory
    // (`latest`, the major `8`, the minor `8.30`, all pointing at the exact
    // version); only real directories count as an installed version here.
    let mut crew_gitleaks_versions: Vec<String> =
        std::fs::read_dir(crew_paths.mise_pool().join("installs").join("gitleaks"))
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.file_type().unwrap().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
    crew_gitleaks_versions.sort();
    assert_eq!(
        crew_gitleaks_versions,
        vec![other.clone()],
        "the crew pool holds only what the crew declared"
    );

    // Spec E §7: "the agent resolves the crew's" version. The agent's own
    // table pins `gitleaks` at the crew's version; only the crew pool holds
    // that exact version, so this proves the agent's pool chain actually
    // reaches the crew pool, not just that `install_level` placed files
    // correctly.
    let paths = layout.agent(&id);
    std::fs::create_dir_all(&paths.root).unwrap();
    let agent_tools = BTreeMap::from([("gitleaks".to_string(), other.clone())]);
    tc.write(&id, &paths, &BTreeMap::new(), &agent_tools, false)
        .unwrap();
    tc.install(&id, &paths).unwrap();

    assert!(
        !paths.mise_data_dir().join("installs").exists()
            || std::fs::read_dir(paths.mise_data_dir().join("installs"))
                .unwrap()
                .next()
                .is_none(),
        "the crew pool already held the pinned version, so nothing landed privately"
    );

    let out = Command::new(&tools.mise)
        .args(["which", "gitleaks"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &paths.home)
        .envs(&mise_env(&id, &paths, &layout))
        .current_dir("/")
        .output()
        .unwrap();
    let resolved = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        out.status.success(),
        "mise which gitleaks: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        resolved.starts_with(&crew_paths.mise_pool().display().to_string()),
        "gitleaks resolved to {resolved}, expected the crew's pin under {}",
        crew_paths.mise_pool().display()
    );
}
