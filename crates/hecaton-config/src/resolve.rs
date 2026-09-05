//! Resolution (spec §5): fold the settings layers for every agent, type the
//! result, validate it, and hand back a `FleetSpec` with no inheritance left.

use std::collections::BTreeMap;

use hecaton_api::{AgentSettings, CrewSpec, FleetSpec};
use hecaton_core::Fleet;
use serde_json::{Value, json};

use crate::ConfigError;
use crate::file::FleetFile;
use crate::merge::merge_layers;
use crate::validate::validate_agent;

/// Inputs to resolution that do not come from the file itself.
#[derive(Debug, Clone, Default)]
pub struct ResolveOptions {
    /// `--name` on the CLI; wins over the file's `name`.
    pub name_override: Option<String>,
    /// The host's `~/.claude/settings.json`, layered beneath `defaults`.
    pub host_claude_settings: Option<Value>,
}

/// Resolves every agent and validates the result.
pub fn resolve(file: &FleetFile, opts: &ResolveOptions) -> Result<FleetSpec, ConfigError> {
    let name = opts
        .name_override
        .clone()
        .or_else(|| file.name.clone())
        .ok_or_else(|| ConfigError::Invalid {
            path: "name".to_string(),
            message: "required; set `name` in the file or pass --name".to_string(),
        })?;
    let host_layer = opts
        .host_claude_settings
        .as_ref()
        .map(|s| json!({ "claude": { "settings": s } }));
    expect_mapping("defaults", &file.defaults)?;

    let mut crews = BTreeMap::new();
    for (crew_name, crew) in &file.crews {
        let crew_path = format!("crews.{crew_name}");
        expect_mapping(&format!("{crew_path}.defaults"), &crew.defaults)?;

        let mut agents = BTreeMap::new();
        for (agent_name, layer) in &crew.agents {
            let agent_path = format!("{crew_path}.agents.{agent_name}");
            expect_mapping(&agent_path, layer)?;
            let merged =
                merge_layers(
                    host_layer
                        .iter()
                        .chain([&file.defaults, &crew.defaults, layer]),
                );
            let settings: AgentSettings =
                serde_json::from_value(merged).map_err(|e| ConfigError::Invalid {
                    path: agent_path.clone(),
                    message: e.to_string(),
                })?;
            validate_agent(&agent_path, &settings)?;
            agents.insert(agent_name.clone(), settings);
        }
        crews.insert(
            crew_name.clone(),
            CrewSpec {
                repo: crew.repo.clone(),
                git_ref: crew.git_ref.clone(),
                git: crew.git.clone(),
                agents,
            },
        );
    }

    let spec = FleetSpec { name, crews };
    Fleet::try_from(spec.clone())?; // names and repos
    Ok(spec)
}

/// A settings layer must be a mapping (or absent/null, which merge treats as empty).
fn expect_mapping(path: &str, v: &Value) -> Result<(), ConfigError> {
    if v.is_object() || v.is_null() {
        Ok(())
    } else {
        Err(ConfigError::Invalid {
            path: path.to_string(),
            message: "expected a mapping".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn file(yaml: &str) -> FleetFile {
        parse(yaml).unwrap()
    }

    fn opts() -> ResolveOptions {
        ResolveOptions {
            name_override: None,
            host_claude_settings: None,
        }
    }

    const BASE: &str = "apiVersion: hecaton/v1\nkind: Fleet\nname: f\ncrews:\n  c:\n    repo: o/r\n    agents:\n      a: {}\n";

    #[test]
    fn name_override_beats_file_name_and_missing_name_errors() {
        let spec = resolve(
            &file(BASE),
            &ResolveOptions {
                name_override: Some("other".into()),
                ..opts()
            },
        )
        .unwrap();
        assert_eq!(spec.name, "other");

        let no_name = "apiVersion: hecaton/v1\nkind: Fleet\ncrews: {}\n";
        let err = resolve(&file(no_name), &opts()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "name: required; set `name` in the file or pass --name"
        );
    }

    #[test]
    fn layers_merge_fleet_then_crew_then_agent() {
        let yaml = r#"
apiVersion: hecaton/v1
kind: Fleet
name: f
defaults:
  claude: { settings: { model: sonnet, theme: dark } }
  tools: { node: "22.11.0" }
crews:
  c:
    repo: o/r
    defaults:
      claude: { settings: { model: opus } }
    agents:
      a: { tools: { node: null, go: "1.23.4" } }
"#;
        let spec = resolve(&file(yaml), &opts()).unwrap();
        let a = &spec.crews["c"].agents["a"];
        assert_eq!(a.claude.settings, json!({"model": "opus", "theme": "dark"}));
        assert_eq!(
            a.tools,
            std::collections::BTreeMap::from([("go".to_string(), "1.23.4".to_string())])
        );
    }

    #[test]
    fn host_claude_settings_sit_beneath_fleet_defaults() {
        let yaml = "apiVersion: hecaton/v1\nkind: Fleet\nname: f\ndefaults:\n  claude: { settings: { model: sonnet } }\ncrews:\n  c:\n    repo: o/r\n    agents:\n      a: {}\n";
        let host = json!({"model": "haiku", "theme": "dark"});
        let spec = resolve(
            &file(yaml),
            &ResolveOptions {
                host_claude_settings: Some(host),
                ..opts()
            },
        )
        .unwrap();
        assert_eq!(
            spec.crews["c"].agents["a"].claude.settings,
            json!({"model": "sonnet", "theme": "dark"})
        );
    }

    #[test]
    fn crew_fields_are_carried_through() {
        let yaml = "apiVersion: hecaton/v1\nkind: Fleet\nname: f\ncrews:\n  c:\n    repo: o/r\n    ref: dev\n    git: { push: false }\n    agents: {}\n";
        let spec = resolve(&file(yaml), &opts()).unwrap();
        assert_eq!(spec.crews["c"].repo, "o/r");
        assert_eq!(spec.crews["c"].git_ref, "dev");
        assert!(!spec.crews["c"].git.push);
    }

    #[test]
    fn unknown_key_in_an_agent_layer_reports_the_agent_path() {
        let yaml = "apiVersion: hecaton/v1\nkind: Fleet\nname: f\ncrews:\n  c:\n    repo: o/r\n    agents:\n      a: { claud: {} }\n";
        let err = resolve(&file(yaml), &opts()).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("crews.c.agents.a: unknown field `claud`"),
            "{err}"
        );
    }

    #[test]
    fn non_mapping_layers_are_rejected_with_their_path() {
        let yaml = "apiVersion: hecaton/v1\nkind: Fleet\nname: f\ndefaults: [1]\ncrews: {}\n";
        assert_eq!(
            resolve(&file(yaml), &opts()).unwrap_err().to_string(),
            "defaults: expected a mapping"
        );
        let yaml = "apiVersion: hecaton/v1\nkind: Fleet\nname: f\ncrews:\n  c:\n    repo: o/r\n    agents:\n      a: 3\n";
        assert_eq!(
            resolve(&file(yaml), &opts()).unwrap_err().to_string(),
            "crews.c.agents.a: expected a mapping"
        );
    }

    #[test]
    fn validation_and_name_errors_propagate_with_paths() {
        let yaml = "apiVersion: hecaton/v1\nkind: Fleet\nname: f\ncrews:\n  c:\n    repo: o/r\n    agents:\n      a: { tools: { node: \"22\" } }\n";
        assert!(
            resolve(&file(yaml), &opts())
                .unwrap_err()
                .to_string()
                .starts_with("crews.c.agents.a.tools.node:")
        );
        let yaml = "apiVersion: hecaton/v1\nkind: Fleet\nname: f\ncrews:\n  c:\n    repo: o/r\n    agents:\n      Bad: {}\n";
        assert!(
            resolve(&file(yaml), &opts())
                .unwrap_err()
                .to_string()
                .starts_with("crews.c.agents.Bad: invalid agent name")
        );
    }
}
