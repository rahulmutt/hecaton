//! Fleet and agent status (spec §7 status model; Phase 2 spec §2.3). Wire
//! types: the daemon stores and returns these.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Seconds since the Unix epoch. Whole seconds are enough for backoff and
/// resync decisions and keep the wire form a plain integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Timestamp(pub u64);

impl Timestamp {
    pub fn plus_secs(self, s: u64) -> Self {
        Self(self.0.saturating_add(s))
    }
}

/// Hex SHA-256 of an agent's resolved spec. Computed in `hecaton-core`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SpecHash(String);

impl SpecHash {
    pub fn new(hex: String) -> Self {
        Self(hex)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FleetPhase {
    Pending,
    Reconciling,
    Ready,
    Degraded,
    Terminating,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentPhase {
    Pending,
    Materializing,
    Starting,
    Ready,
    Dead,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub phase: AgentPhase,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub last_event_at: Option<Timestamp>,
    #[serde(default)]
    pub applied_hash: Option<SpecHash>,
    #[serde(default)]
    pub restarts: u32,
    #[serde(default)]
    pub next_restart_at: Option<Timestamp>,
}

impl Default for AgentStatus {
    fn default() -> Self {
        Self {
            phase: AgentPhase::Pending,
            message: String::new(),
            last_event_at: None,
            applied_hash: None,
            restarts: 0,
            next_restart_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetStatus {
    pub generation: u64,
    pub observed_generation: u64,
    pub phase: FleetPhase,
    /// Keyed by `AgentId` display form `fleet/crew/agent`.
    #[serde(default)]
    pub agents: BTreeMap<String, AgentStatus>,
}

impl Default for FleetStatus {
    fn default() -> Self {
        Self {
            generation: 0,
            observed_generation: 0,
            phase: FleetPhase::Pending,
            agents: BTreeMap::new(),
        }
    }
}

impl FleetStatus {
    /// The status entry for `id`, created as `Pending` if absent.
    pub fn entry(&mut self, id: &str) -> &mut AgentStatus {
        self.agents.entry(id.to_string()).or_default()
    }
}

/// One row of `GET /v1/fleets`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetSummary {
    pub name: String,
    pub phase: FleetPhase,
    pub generation: u64,
    pub observed_generation: u64,
    pub agents: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn timestamp_addition_saturates() {
        assert_eq!(Timestamp(10).plus_secs(5), Timestamp(15));
        assert_eq!(Timestamp(u64::MAX).plus_secs(5), Timestamp(u64::MAX));
    }

    #[test]
    fn phases_serialize_lowercase() {
        assert_eq!(
            serde_json::to_value(FleetPhase::Degraded).unwrap(),
            json!("degraded")
        );
        assert_eq!(
            serde_json::to_value(AgentPhase::Materializing).unwrap(),
            json!("materializing")
        );
    }

    #[test]
    fn entry_inserts_a_pending_default() {
        let mut s = FleetStatus::default();
        s.entry("f/c/a").restarts = 3;
        assert_eq!(s.agents["f/c/a"].phase, AgentPhase::Pending);
        assert_eq!(s.agents["f/c/a"].restarts, 3);
        assert_eq!(
            s.entry("f/c/a").restarts,
            3,
            "second call returns the same entry"
        );
    }

    #[test]
    fn status_round_trips_and_tolerates_missing_optional_fields() {
        let s: FleetStatus = serde_json::from_value(json!({
            "generation": 2, "observed_generation": 1, "phase": "reconciling",
            "agents": { "f/c/a": { "phase": "starting" } }
        }))
        .unwrap();
        assert_eq!(s.agents["f/c/a"].applied_hash, None);
        let back: FleetStatus = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn down_is_a_phase_and_summaries_round_trip() {
        assert_eq!(
            serde_json::to_value(FleetPhase::Down).unwrap(),
            json!("down")
        );
        let s = FleetSummary {
            name: "payments".into(),
            phase: FleetPhase::Ready,
            generation: 3,
            observed_generation: 3,
            agents: 2,
        };
        let back: FleetSummary = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
