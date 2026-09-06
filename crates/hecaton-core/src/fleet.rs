//! The domain fleet: the wire `FleetSpec` after every name and repo has
//! been validated ("parse, don't validate").

use std::collections::BTreeMap;

use hecaton_api::{AgentSettings, CrewSpec, FleetSpec, GitSettings};

use crate::name::{AgentName, CrewName, FleetName, NameError};
use crate::repo::{RepoError, RepoRef};

/// A validated fleet.
#[derive(Debug, Clone, PartialEq)]
pub struct Fleet {
    pub name: FleetName,
    pub crews: BTreeMap<CrewName, Crew>,
}

/// A validated crew.
#[derive(Debug, Clone, PartialEq)]
pub struct Crew {
    pub repo: RepoRef,
    pub git_ref: String,
    pub git: GitSettings,
    pub agents: BTreeMap<AgentName, AgentSettings>,
}

/// A spec that failed validation. The message always starts with the
/// config path of the offending value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FleetError {
    #[error("{path}: {source}")]
    InvalidName { path: String, source: NameError },
    #[error("{path}: {source}")]
    InvalidRepo { path: String, source: RepoError },
    #[error("{path}: must not be empty")]
    EmptyRef { path: String },
    #[error("{path}: reserved name (tmux anchor window)")]
    ReservedAgentName { path: String },
    #[error("{path}: must not be empty")]
    EmptyIdentity { path: String },
}

/// The tmux anchor window that keeps a crew's session alive is named
/// `hecaton` (spec §4.3); an agent may not claim that name.
pub const RESERVED_AGENT_NAME: &str = "hecaton";

impl TryFrom<FleetSpec> for Fleet {
    type Error = FleetError;

    fn try_from(spec: FleetSpec) -> Result<Self, FleetError> {
        let name = FleetName::try_from(spec.name).map_err(|source| FleetError::InvalidName {
            path: "name".to_string(),
            source,
        })?;
        let mut crews = BTreeMap::new();
        for (crew_name, crew) in spec.crews {
            let path = format!("crews.{crew_name}");
            let crew_name =
                CrewName::try_from(crew_name).map_err(|source| FleetError::InvalidName {
                    path: path.clone(),
                    source,
                })?;
            crews.insert(crew_name, convert_crew(&path, crew)?);
        }
        Ok(Self { name, crews })
    }
}

fn convert_crew(path: &str, crew: CrewSpec) -> Result<Crew, FleetError> {
    let repo = RepoRef::parse(&crew.repo).map_err(|source| FleetError::InvalidRepo {
        path: format!("{path}.repo"),
        source,
    })?;
    if crew.git_ref.is_empty() {
        return Err(FleetError::EmptyRef {
            path: format!("{path}.ref"),
        });
    }
    if let Some(identity) = &crew.git.identity {
        for (field, value) in [("name", &identity.name), ("email", &identity.email)] {
            if value.trim().is_empty() {
                return Err(FleetError::EmptyIdentity {
                    path: format!("{path}.git.identity.{field}"),
                });
            }
        }
    }
    let mut agents = BTreeMap::new();
    for (agent_name, settings) in crew.agents {
        let agent_path = format!("{path}.agents.{agent_name}");
        if agent_name == RESERVED_AGENT_NAME {
            return Err(FleetError::ReservedAgentName { path: agent_path });
        }
        let agent_name =
            AgentName::try_from(agent_name).map_err(|source| FleetError::InvalidName {
                path: agent_path,
                source,
            })?;
        agents.insert(agent_name, settings);
    }
    Ok(Crew {
        repo,
        git_ref: crew.git_ref,
        git: crew.git,
        agents,
    })
}

impl From<Fleet> for FleetSpec {
    fn from(fleet: Fleet) -> Self {
        Self {
            name: fleet.name.into(),
            crews: fleet
                .crews
                .into_iter()
                .map(|(name, crew)| {
                    (
                        name.into(),
                        CrewSpec {
                            repo: crew.repo.clone_url(),
                            git_ref: crew.git_ref,
                            git: crew.git,
                            agents: crew
                                .agents
                                .into_iter()
                                .map(|(n, s)| (n.into(), s))
                                .collect(),
                        },
                    )
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hecaton_api::{AgentSettings, CrewSpec, FleetSpec, GitSettings};
    use std::collections::BTreeMap;

    fn spec(fleet: &str, crew: &str, repo: &str, git_ref: &str, agents: &[&str]) -> FleetSpec {
        FleetSpec {
            name: fleet.into(),
            crews: BTreeMap::from([(
                crew.to_string(),
                CrewSpec {
                    repo: repo.into(),
                    git_ref: git_ref.into(),
                    git: GitSettings::default(),
                    agents: agents
                        .iter()
                        .map(|a| (a.to_string(), AgentSettings::default()))
                        .collect(),
                },
            )]),
        }
    }

    #[test]
    fn converts_a_valid_spec() {
        let f = Fleet::try_from(spec(
            "payments",
            "backend",
            "acme/api",
            "main",
            &["alice", "bob"],
        ))
        .unwrap();
        assert_eq!(f.name.as_str(), "payments");
        let crew = &f.crews[&CrewName::try_from("backend").unwrap()];
        assert_eq!(
            crew.repo,
            RepoRef::GitHub {
                owner: "acme".into(),
                name: "api".into()
            }
        );
        assert_eq!(crew.git_ref, "main");
        assert_eq!(crew.agents.len(), 2);
    }

    #[test]
    fn invalid_fleet_name_reports_path_name() {
        let err =
            Fleet::try_from(spec("Payments", "backend", "acme/api", "main", &[])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "name: invalid fleet name \"Payments\": contains characters other than a-z, 0-9 and '-'"
        );
    }

    #[test]
    fn invalid_crew_and_agent_names_report_their_paths() {
        let err = Fleet::try_from(spec("payments", "Back", "acme/api", "main", &[])).unwrap_err();
        assert!(
            err.to_string().starts_with("crews.Back: invalid crew name"),
            "{err}"
        );

        let err =
            Fleet::try_from(spec("payments", "backend", "acme/api", "main", &["Bob"])).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("crews.backend.agents.Bob: invalid agent name"),
            "{err}"
        );
    }

    #[test]
    fn the_anchor_window_name_is_reserved() {
        let err = Fleet::try_from(spec(
            "payments",
            "backend",
            "acme/api",
            "main",
            &["hecaton"],
        ))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "crews.backend.agents.hecaton: reserved name (tmux anchor window)"
        );
    }

    #[test]
    fn invalid_repo_and_empty_ref_report_their_paths() {
        let err = Fleet::try_from(spec("payments", "backend", "nope", "main", &[])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "crews.backend.repo: invalid repo \"nope\": expected owner/name or a clone URL"
        );

        let err = Fleet::try_from(spec("payments", "backend", "acme/api", "", &[])).unwrap_err();
        assert_eq!(err.to_string(), "crews.backend.ref: must not be empty");
    }

    #[test]
    fn round_trips_back_to_the_wire_type() {
        let original = spec("payments", "backend", "acme/api", "main", &["alice"]);
        let back: FleetSpec = Fleet::try_from(original.clone()).unwrap().into();
        assert_eq!(back.name, original.name);
        // repo is normalized to the https clone URL on the way back
        assert_eq!(
            back.crews["backend"].repo,
            "https://github.com/acme/api.git"
        );
        assert_eq!(
            back.crews["backend"].agents,
            original.crews["backend"].agents
        );
    }

    #[test]
    fn an_empty_identity_field_reports_its_path() {
        let mut s = spec("f", "c", "acme/x", "main", &["a"]);
        s.crews.get_mut("c").unwrap().git.identity = Some(hecaton_api::GitIdentity {
            name: "".into(),
            email: "a@b.c".into(),
        });
        assert_eq!(
            Fleet::try_from(s.clone()).unwrap_err().to_string(),
            "crews.c.git.identity.name: must not be empty"
        );
        s.crews.get_mut("c").unwrap().git.identity = Some(hecaton_api::GitIdentity {
            name: "A".into(),
            email: " ".into(),
        });
        assert_eq!(
            Fleet::try_from(s).unwrap_err().to_string(),
            "crews.c.git.identity.email: must not be empty"
        );
    }
}
