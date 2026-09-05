//! Client-side checks on a resolved agent (spec §5 "Validation"). Names and
//! repos are validated by `hecaton_core::Fleet`; this covers everything else.

use hecaton_api::AgentSettings;
use serde_json::Value;

use crate::ConfigError;

/// Environment variables hecaton sets itself (spec §4 table); user `env`
/// may not override them. Entries ending in `_` are prefixes.
pub const RESERVED_ENV_PREFIXES: &[&str] = &[
    "HOME",
    "XDG_",
    "CLAUDE_CONFIG_DIR",
    "GH_CONFIG_DIR",
    "MISE_",
    "HECATON_",
    "PATH",
];

/// Rejects mise's fuzzy forms: keywords, `prefix:`/`ref:`/`path:`/`sub-`
/// specs, wildcards, and bare `major` / `major.minor` numbers.
pub fn is_exact_version(v: &str) -> bool {
    if v.is_empty() || matches!(v, "latest" | "lts" | "system") {
        return false;
    }
    if v.ends_with(".x") || v.contains('*') || v.contains(':') {
        return false;
    }
    let numeric_parts = v
        .split('.')
        .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .count();
    let total_parts = v.split('.').count();
    // "22" and "22.11" are fuzzy; "3.7c", "1.0.0-rc.1", "v1.2.3" are exact.
    !(numeric_parts == total_parts && total_parts < 3)
}

/// Validates one resolved settings block; `path` prefixes every message.
pub fn validate_agent(path: &str, settings: &AgentSettings) -> Result<(), ConfigError> {
    let invalid = |suffix: &str, message: String| ConfigError::Invalid {
        path: format!("{path}.{suffix}"),
        message,
    };

    for (tool, version) in &settings.tools {
        if !is_exact_version(version) {
            return Err(invalid(
                &format!("tools.{tool}"),
                format!(
                    "expected an exact version, got {version:?} (try: mise latest {tool}@{version})"
                ),
            ));
        }
    }

    let Value::Object(claude_settings) = &settings.claude.settings else {
        return Err(invalid("claude.settings", "expected a mapping".to_string()));
    };
    if claude_settings.contains_key("hooks") {
        return Err(invalid(
            "claude.settings.hooks",
            "hecaton owns this key; configure hook behaviour via `flow` instead".to_string(),
        ));
    }
    if !settings.sandbox.is_object() {
        return Err(invalid("sandbox", "expected a mapping".to_string()));
    }
    if !settings.flow.is_object() {
        return Err(invalid("flow", "expected a mapping".to_string()));
    }

    for key in settings.env.keys() {
        if key.is_empty() {
            return Err(invalid("env", "empty variable name".to_string()));
        }
        let reserved = RESERVED_ENV_PREFIXES.iter().any(|r| {
            if r.ends_with('_') {
                key.starts_with(r)
            } else {
                key == r
            }
        });
        if reserved {
            return Err(invalid(
                &format!("env.{key}"),
                "reserved; hecaton sets this variable".to_string(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hecaton_api::{AgentSettings, ClaudeSettings};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn exact_versions_are_accepted() {
        for v in [
            "22.11.0",
            "3.7c",
            "2026.9.1",
            "0.75.0",
            "2.1.261",
            "1.0.0-rc.1",
            "v1.2.3",
            "8.30.1",
        ] {
            assert!(is_exact_version(v), "{v:?} should be exact");
        }
    }

    #[test]
    fn fuzzy_versions_are_rejected() {
        for v in [
            "22",
            "22.11",
            "latest",
            "lts",
            "system",
            "22.x",
            "22.11.x",
            "22*",
            "prefix:22",
            "ref:main",
            "sub-1:latest",
            "path:/x",
            "",
        ] {
            assert!(!is_exact_version(v), "{v:?} should be fuzzy");
        }
    }

    fn with_tools(tools: &[(&str, &str)]) -> AgentSettings {
        AgentSettings {
            tools: tools
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..AgentSettings::default()
        }
    }

    #[test]
    fn happy_path_passes() {
        let s = AgentSettings {
            claude: ClaudeSettings {
                settings: json!({"model": "opus"}),
                ..ClaudeSettings::default()
            },
            env: BTreeMap::from([("RUST_LOG".to_string(), "info".to_string())]),
            ..with_tools(&[("node", "22.11.0")])
        };
        validate_agent("crews.backend.agents.bob", &s).unwrap();
    }

    #[test]
    fn fuzzy_tool_version_error_names_the_path_and_hints_mise_latest() {
        let err =
            validate_agent("crews.backend.agents.bob", &with_tools(&[("node", "22")])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "crews.backend.agents.bob.tools.node: expected an exact version, got \"22\" (try: mise latest node@22)"
        );
    }

    #[test]
    fn hooks_in_claude_settings_are_rejected() {
        let s = AgentSettings {
            claude: ClaudeSettings {
                settings: json!({"hooks": {}}),
                ..ClaudeSettings::default()
            },
            ..AgentSettings::default()
        };
        let err = validate_agent("crews.c.agents.a", &s).unwrap_err();
        assert_eq!(
            err.to_string(),
            "crews.c.agents.a.claude.settings.hooks: hecaton owns this key; configure hook behaviour via `flow` instead"
        );
    }

    #[test]
    fn passthrough_blocks_must_be_maps() {
        let s = AgentSettings {
            sandbox: json!([1]),
            ..AgentSettings::default()
        };
        assert_eq!(
            validate_agent("p", &s).unwrap_err().to_string(),
            "p.sandbox: expected a mapping"
        );
        let s = AgentSettings {
            flow: json!("x"),
            ..AgentSettings::default()
        };
        assert_eq!(
            validate_agent("p", &s).unwrap_err().to_string(),
            "p.flow: expected a mapping"
        );
        let s = AgentSettings {
            claude: ClaudeSettings {
                settings: json!(3),
                ..ClaudeSettings::default()
            },
            ..AgentSettings::default()
        };
        assert_eq!(
            validate_agent("p", &s).unwrap_err().to_string(),
            "p.claude.settings: expected a mapping"
        );
    }

    #[test]
    fn reserved_env_keys_are_rejected() {
        for key in [
            "HOME",
            "XDG_DATA_HOME",
            "CLAUDE_CONFIG_DIR",
            "GH_CONFIG_DIR",
            "MISE_DATA_DIR",
            "HECATON_FLEET",
            "PATH",
        ] {
            let s = AgentSettings {
                env: BTreeMap::from([(key.to_string(), "x".to_string())]),
                ..AgentSettings::default()
            };
            let err = validate_agent("p", &s).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("p.env.{key}: reserved; hecaton sets this variable"),
                "for {key}"
            );
        }
        let s = AgentSettings {
            env: BTreeMap::from([(String::new(), "x".to_string())]),
            ..AgentSettings::default()
        };
        assert_eq!(
            validate_agent("p", &s).unwrap_err().to_string(),
            "p.env: empty variable name"
        );
    }
}
