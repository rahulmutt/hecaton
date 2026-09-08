//! What the index shows (plugins spec §18.5): the agents enabled for web,
//! joined with the latest `fleets/watch` frame. The plugin never calls
//! `GET fleets`; an index that is right at all proves the watch path.

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard};

use hecaton_api::{AgentPhase, FleetRecord};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRow {
    pub id: String,
    pub phase: AgentPhase,
    #[serde(default)]
    pub message: String,
}

#[derive(Default)]
struct Inner {
    enabled: BTreeSet<String>,
    fleets: Vec<FleetRecord>,
}

#[derive(Default)]
pub struct Cache {
    inner: Mutex<Inner>,
}

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `activate`: listed when enabled, hidden otherwise. The pair stays
    /// active either way; only the listing changes.
    pub fn set_enabled(&self, agent: &str, enabled: bool) {
        let mut i = self.lock();
        if enabled {
            i.enabled.insert(agent.to_string());
        } else {
            i.enabled.remove(agent);
        }
    }

    /// `deactivate`: forget the agent. Today that is the listing flag
    /// alone, the same as `set_enabled(agent, false)`; any per-agent state
    /// added later is purged here and kept there.
    pub fn remove(&self, agent: &str) {
        self.lock().enabled.remove(agent);
    }

    pub fn is_enabled(&self, agent: &str) -> bool {
        self.lock().enabled.contains(agent)
    }

    /// One `fleets/watch` frame: the whole list, replaced.
    pub fn set_fleets(&self, fleets: Vec<FleetRecord>) {
        self.lock().fleets = fleets;
    }

    pub fn rows(&self) -> Vec<AgentRow> {
        let i = self.lock();
        rows_of(&i.enabled, &i.fleets)
    }
}

/// The enabled agents in id order with their phase from the fleets;
/// an enabled agent no fleet knows yet is `pending` with no message.
/// Agent ids are fleet-prefixed (`<fleet>/<crew>/<agent>`), so at most
/// one fleet knows an id; the first match is the only one.
pub fn rows_of(enabled: &BTreeSet<String>, fleets: &[FleetRecord]) -> Vec<AgentRow> {
    enabled
        .iter()
        .map(|id| {
            let status = fleets.iter().find_map(|f| f.status.agents.get(id));
            AgentRow {
                id: id.clone(),
                phase: status.map_or(AgentPhase::Pending, |s| s.phase),
                message: status.map(|s| s.message.clone()).unwrap_or_default(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hecaton_api::FleetSpec;
    use std::collections::BTreeMap;

    fn fleet(name: &str, agents: &[(&str, AgentPhase, &str)]) -> FleetRecord {
        let mut r = FleetRecord::new(FleetSpec {
            name: name.into(),
            crews: BTreeMap::new(),
        });
        for (id, phase, message) in agents {
            let s = r.status.entry(id);
            s.phase = *phase;
            s.message = (*message).to_string();
        }
        r
    }

    #[test]
    fn rows_are_the_enabled_agents_with_phases_from_the_fleets() {
        let c = Cache::new();
        c.set_enabled("f/c/b", true);
        c.set_enabled("f/c/a", true);
        c.set_enabled("f/c/z", false);
        assert!(c.is_enabled("f/c/a") && !c.is_enabled("f/c/z"));
        assert_eq!(
            c.rows(),
            vec![
                AgentRow {
                    id: "f/c/a".into(),
                    phase: AgentPhase::Pending,
                    message: String::new()
                },
                AgentRow {
                    id: "f/c/b".into(),
                    phase: AgentPhase::Pending,
                    message: String::new()
                },
            ],
            "no fleets yet: pending"
        );
        c.set_fleets(vec![
            fleet(
                "f",
                &[
                    ("f/c/a", AgentPhase::Ready, ""),
                    ("f/c/c", AgentPhase::Ready, ""),
                ],
            ),
            fleet("g", &[("f/c/b", AgentPhase::Dead, "exit 1")]),
        ]);
        let rows = c.rows();
        assert_eq!(rows[0].phase, AgentPhase::Ready);
        assert_eq!(
            (rows[1].phase, rows[1].message.as_str()),
            (AgentPhase::Dead, "exit 1")
        );
        assert_eq!(rows.len(), 2, "c is not enabled");
        c.remove("f/c/a");
        c.set_enabled("f/c/b", false);
        assert!(c.rows().is_empty());
        assert_eq!(
            serde_json::to_value(AgentRow {
                id: "x".into(),
                phase: AgentPhase::Ready,
                message: "m".into()
            })
            .unwrap(),
            serde_json::json!({ "id": "x", "phase": "ready", "message": "m" })
        );
    }
}
