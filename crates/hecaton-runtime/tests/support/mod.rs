//! Shared helpers for the integration tests that drive real tools.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use hecaton_runtime::{StateLayout, ToolPaths};

/// Tools from the test process's PATH (mise puts the pinned ones there).
pub fn tools() -> Option<ToolPaths> {
    ToolPaths::discover_in(&std::env::var_os("PATH").unwrap_or_default()).ok()
}

/// Returns `true` if the test may run. Otherwise prints a skip reason, or
/// panics when `HECATON_REQUIRE_TOOLS=1` (CI never skips).
pub fn require_or_skip(name: &str, present: bool) -> bool {
    if present {
        return true;
    }
    if std::env::var_os("HECATON_REQUIRE_TOOLS").is_some_and(|v| v == "1") {
        panic!("{name} is required (HECATON_REQUIRE_TOOLS=1) but not available");
    }
    eprintln!("skip: {name} not available");
    false
}

/// A fresh directory under `target/tmp`. Deliberately not `/tmp`: nono's
/// built-in groups grant `/tmp`, so a sandbox-escape assertion there would
/// pass vacuously.
pub fn temp_root(test: &str) -> PathBuf {
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

pub fn layout(root: &Path) -> StateLayout {
    StateLayout {
        state_root: root.join("state"),
        data_root: root.join("data"),
        config_root: root.join("config"),
    }
}

/// True when Landlock is usable: `nono run` of `true` succeeds. `--allow-cwd`
/// is required because nono 0.75.0 refuses CWD access non-interactively
/// without it; without the flag the probe fails even though Landlock itself
/// is fine.
pub fn landlock_works(tools: &ToolPaths, root: &Path) -> bool {
    let home = root.join("nono-probe-home");
    std::fs::create_dir_all(&home).unwrap();
    std::process::Command::new(&tools.nono)
        .args(["-s", "run", "--allow-cwd", "--", "/bin/true"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .current_dir(root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
