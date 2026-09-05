//! The resolved fleet specification — the wire format for `up`/`update`
//! and what the daemon stores (spec §5, §7).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::settings::AgentSettings;

/// A fully-resolved fleet: every agent carries a complete settings block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetSpec {
    pub name: String,
    #[serde(default)]
    pub crews: BTreeMap<String, CrewSpec>,
}

/// One crew: a shared repository plus its agents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrewSpec {
    /// GitHub `owner/name` shorthand or a full clone URL.
    pub repo: String,
    /// Base ref for per-agent branches.
    #[serde(rename = "ref")]
    pub git_ref: String,
    #[serde(default)]
    pub git: GitSettings,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentSettings>,
}

/// Git permissions for a crew.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSettings {
    #[serde(default = "default_true")]
    pub push: bool,
    #[serde(default)]
    pub auth: GitAuth,
}

impl Default for GitSettings {
    fn default() -> Self {
        Self {
            push: true,
            auth: GitAuth::default(),
        }
    }
}

/// Where the crew's git credentials come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitAuth {
    /// Token from the host's `gh` config, sent in the credential bundle.
    #[default]
    Gh,
    /// No credentials; public repos only.
    None,
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn git_settings_default_to_push_with_gh_auth() {
        let g = GitSettings::default();
        assert!(g.push);
        assert_eq!(g.auth, GitAuth::Gh);
    }

    #[test]
    fn crew_spec_uses_ref_as_the_wire_name() {
        let c: CrewSpec = serde_json::from_value(json!({
            "repo": "acme/payments-api", "ref": "main", "git": { "push": false, "auth": "none" },
            "agents": { "alice": {} }
        }))
        .unwrap();
        assert_eq!(c.git_ref, "main");
        assert!(!c.git.push);
        assert_eq!(c.git.auth, GitAuth::None);
        assert!(c.agents.contains_key("alice"));
        let back = serde_json::to_value(&c).unwrap();
        assert_eq!(back["ref"], "main");
        assert_eq!(back["git"]["auth"], "none");
    }

    #[test]
    fn fleet_spec_round_trips() {
        let spec = FleetSpec {
            name: "payments".into(),
            crews: BTreeMap::from([(
                "backend".to_string(),
                CrewSpec {
                    repo: "acme/payments-api".into(),
                    git_ref: "main".into(),
                    git: GitSettings::default(),
                    agents: BTreeMap::from([("alice".to_string(), AgentSettings::default())]),
                },
            )]),
        };
        let back: FleetSpec = serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(back, spec);
    }
}
