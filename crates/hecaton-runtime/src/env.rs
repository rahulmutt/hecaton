//! The agent's environment (spec §4 table). These become the nono profile's
//! `environment.set_vars`; `PATH` is set by nono and `mise exec`.

use std::collections::BTreeMap;

use hecaton_core::AgentId;

use crate::layout::{AgentPaths, StateLayout};

pub fn agent_env(
    id: &AgentId,
    paths: &AgentPaths,
    layout: &StateLayout,
    api_url: &str,
    hook_secret: &str,
    user_env: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let s = |p: &std::path::Path| p.display().to_string();
    let mut env: BTreeMap<String, String> = BTreeMap::from([
        ("HOME".to_string(), s(&paths.home)),
        ("XDG_CONFIG_HOME".to_string(), s(&paths.xdg_config())),
        ("XDG_DATA_HOME".to_string(), s(&paths.xdg_data())),
        ("XDG_STATE_HOME".to_string(), s(&paths.xdg_state())),
        ("XDG_CACHE_HOME".to_string(), s(&paths.xdg_cache())),
        ("CLAUDE_CONFIG_DIR".to_string(), s(&paths.claude_dir())),
        ("GH_CONFIG_DIR".to_string(), s(&paths.gh_dir())),
        ("MISE_GLOBAL_CONFIG_FILE".to_string(), s(&paths.mise_toml)),
        // The in-sandbox counterpart of the `cwd=/` the daemon's `mise
        // install` uses: mise otherwise walks up from the workspace and
        // applies every `mise.toml` it finds. The repository's own file is
        // applied even though `mise trust` never covered it (mise 2026.9.1
        // honours `[tools]` from an untrusted config), which pulls in tools
        // the daemon never installed and the sandbox cannot download; an
        // ancestor file that the profile does not grant is worse still —
        // mise exits on the read error. A ceiling at the workspace stops the
        // walk before either, leaving only the generated global file. This
        // ceiling only stops the upward walk: a `mise.toml` in a
        // subdirectory of the workspace is still discovered if the agent
        // `cd`s there, same as it would be for any other mise invocation —
        // and the sandbox has no network for installs either way, so a
        // subdirectory config naming an uninstalled tool fails the same way
        // the repository's own file would.
        ("MISE_CEILING_PATHS".to_string(), s(&paths.workspace)),
        ("MISE_DATA_DIR".to_string(), s(&layout.mise_data_dir())),
        ("MISE_CONFIG_DIR".to_string(), s(&paths.mise_config_dir())),
        ("MISE_STATE_DIR".to_string(), s(&paths.mise_state_dir())),
        ("MISE_CACHE_DIR".to_string(), s(&paths.mise_cache_dir())),
        ("HECATON_FLEET".to_string(), id.fleet.to_string()),
        ("HECATON_CREW".to_string(), id.crew.to_string()),
        ("HECATON_AGENT".to_string(), id.agent.to_string()),
        ("HECATON_AGENT_ID".to_string(), id.to_string()),
        ("HECATON_API_URL".to_string(), api_url.to_string()),
        // What `hecaton hook-relay` presents; the reserved `HECATON_` prefix
        // keeps user `env` away from it.
        ("HECATON_HOOK_SECRET".to_string(), hook_secret.to_string()),
    ]);
    // Reserved keys were rejected at config validation; `entry().or_insert`
    // keeps the isolation rows authoritative even so.
    for (k, v) in user_env {
        env.entry(k.clone()).or_insert_with(|| v.clone());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn isolation_rows_come_first_and_user_env_cannot_override_them() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let id: AgentId = "payments/backend/alice".parse().unwrap();
        let paths = layout.agent(&id);
        let user = BTreeMap::from([
            ("RUST_LOG".to_string(), "info".to_string()),
            ("HOME".to_string(), "/evil".to_string()),
        ]);
        let env = agent_env(
            &id,
            &paths,
            &layout,
            "http://127.0.0.1:7643",
            "hook-s3",
            &user,
        );
        assert_eq!(
            env["HOME"],
            "/h/.local/state/hecaton/fleets/payments/crews/backend/agents/alice/home"
        );
        assert_eq!(env["RUST_LOG"], "info");
        assert_eq!(env["HECATON_AGENT_ID"], "payments/backend/alice");
        assert_eq!(env["MISE_DATA_DIR"], "/h/.local/share/hecaton/mise");
        assert_eq!(env["HECATON_HOOK_SECRET"], "hook-s3");
        assert_eq!(
            env["MISE_CEILING_PATHS"],
            "/h/.local/state/hecaton/fleets/payments/crews/backend/agents/alice/workspace"
        );
        assert!(!env.contains_key("PATH"), "PATH is nono's");
        assert_eq!(env.len(), 20);
    }
}
