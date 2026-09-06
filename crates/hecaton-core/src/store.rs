//! What the daemon persists per fleet, and the port it persists through
//! (Phase 3 spec §2). The registry in memory is authoritative while the
//! daemon runs; the store is how it survives a restart.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use hecaton_api::{CredentialBundle, FleetPhase, FleetSpec, FleetStatus, FleetSummary};
use serde::{Deserialize, Serialize};

use crate::name::FleetName;
use crate::ports::Keep;

/// One fleet as the daemon stores and returns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetRecord {
    pub spec: FleetSpec,
    pub generation: u64,
    pub desired: Desired,
    pub status: FleetStatus,
}

/// Whether the fleet should be running (P3-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Desired {
    Up,
    Down { keep: Keep, purge: bool },
}

impl FleetRecord {
    pub fn new(spec: FleetSpec) -> Self {
        Self {
            spec,
            generation: 0,
            desired: Desired::Up,
            status: FleetStatus::default(),
        }
    }
    pub fn name(&self) -> &str {
        &self.spec.name
    }
    /// Downed and settled: `up` may re-apply in place, `POST` is not a 409.
    pub fn is_down(&self) -> bool {
        matches!(self.desired, Desired::Down { .. }) && self.status.phase == FleetPhase::Down
    }
    pub fn summary(&self) -> FleetSummary {
        FleetSummary {
            name: self.spec.name.clone(),
            phase: self.status.phase,
            generation: self.generation,
            observed_generation: self.status.observed_generation,
            agents: self.status.agents.len(),
        }
    }
}

/// Everything secret about a fleet: the credential bundle and one hook
/// secret per agent id. Encrypted at rest by the store.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FleetSecrets {
    #[serde(default)]
    pub credentials: CredentialBundle,
    /// `fleet/crew/agent` → bearer secret its hooks present.
    #[serde(default)]
    pub hook_secrets: BTreeMap<String, String>,
}

impl fmt::Debug for FleetSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FleetSecrets {{ credentials: {:?}, hook_secrets: {} <redacted> }}",
            self.credentials,
            self.hook_secrets.len()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Corrupt { path: PathBuf, message: String },
}

/// Persists fleets. `put` is atomic per fleet; `purge` removes the fleet's
/// whole directory, kept repos and homes included (`down --purge`). There is
/// no narrower delete: a downed fleet keeps its record until purged (P3-5).
pub trait FleetStore: Send + Sync {
    fn load_all(&self) -> Result<Vec<(FleetRecord, FleetSecrets)>, StoreError>;
    fn put(&self, record: &FleetRecord, secrets: &FleetSecrets) -> Result<(), StoreError>;
    fn purge(&self, name: &FleetName) -> Result<(), StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use hecaton_api::{FleetPhase, FleetSpec};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn spec() -> FleetSpec {
        FleetSpec {
            name: "payments".into(),
            crews: BTreeMap::new(),
        }
    }

    #[test]
    fn a_new_record_is_up_at_generation_zero() {
        let r = FleetRecord::new(spec());
        assert_eq!(r.generation, 0);
        assert_eq!(r.desired, Desired::Up);
        assert_eq!(r.status.phase, FleetPhase::Pending);
        assert_eq!(r.name(), "payments");
        assert!(!r.is_down());
        let s = r.summary();
        assert_eq!(
            (s.name.as_str(), s.generation, s.agents),
            ("payments", 0, 0)
        );
    }

    #[test]
    fn desired_serializes_with_a_state_tag() {
        assert_eq!(
            serde_json::to_value(Desired::Up).unwrap(),
            json!({ "state": "up" })
        );
        let d = Desired::Down {
            keep: Keep {
                repos: true,
                sessions: false,
            },
            purge: false,
        };
        let v = serde_json::to_value(d).unwrap();
        assert_eq!(v["state"], "down");
        assert_eq!(v["keep"]["repos"], true);
        let back: Desired = serde_json::from_value(v).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn is_down_needs_both_the_desire_and_the_phase() {
        let mut r = FleetRecord::new(spec());
        r.desired = Desired::Down {
            keep: Keep::default(),
            purge: false,
        };
        assert!(!r.is_down(), "still terminating");
        r.status.phase = FleetPhase::Down;
        assert!(r.is_down());
    }

    #[test]
    fn secrets_debug_is_redacted_and_round_trips() {
        let s = FleetSecrets {
            credentials: CredentialBundle {
                gh_token: Some("gho_SECRET".into()),
                ..CredentialBundle::default()
            },
            hook_secrets: BTreeMap::from([("f/c/a".to_string(), "hook-SECRET".to_string())]),
        };
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("SECRET"), "{dbg}");
        assert!(dbg.contains("hook_secrets: 1 <redacted>"));
        let back: FleetSecrets = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        let empty: FleetSecrets = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, FleetSecrets::default());
    }

    #[test]
    fn store_errors_start_with_the_path() {
        let e = StoreError::Corrupt {
            path: "/x/fleet.json".into(),
            message: "expected value at line 1".into(),
        };
        assert_eq!(e.to_string(), "/x/fleet.json: expected value at line 1");
    }
}
