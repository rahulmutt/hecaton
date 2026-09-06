//! Prometheus series (architecture spec §8 table). Gauges are recomputed
//! from snapshots on every `/metrics` scrape; counters and histograms are
//! bumped where the event happens.

use std::sync::Arc;

use hecaton_core::{AgentId, FleetRecord};
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry, TextEncoder,
};

#[derive(Clone)]
pub struct Metrics {
    inner: Arc<Inner>,
}

struct Inner {
    registry: Registry,
    fleets: IntGaugeVec,
    agents: IntGaugeVec,
    reconcile_duration: HistogramVec,
    reconcile_errors: IntCounterVec,
    agent_restarts: IntCounterVec,
    hook_events: IntCounterVec,
    hook_handle_duration: HistogramVec,
    /// Always zero until Spec B ships actions; registered so dashboards
    /// can be built now.
    #[allow(dead_code)]
    hook_actions: IntCounterVec,
}

/// Lowercase phase label, the same spelling as the wire form.
fn label<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

impl Metrics {
    pub fn new() -> Result<Self, prometheus::Error> {
        let registry = Registry::new();
        let fleets = IntGaugeVec::new(Opts::new("hecaton_fleets", "Fleets by phase"), &["phase"])?;
        let agents = IntGaugeVec::new(
            Opts::new("hecaton_agents", "Agents by fleet, crew and phase"),
            &["fleet", "crew", "phase"],
        )?;
        let reconcile_duration = HistogramVec::new(
            HistogramOpts::new(
                "hecaton_reconcile_duration_seconds",
                "Reconcile pass duration",
            ),
            &["fleet"],
        )?;
        let reconcile_errors = IntCounterVec::new(
            Opts::new(
                "hecaton_reconcile_errors_total",
                "Passes with a failed step or observe",
            ),
            &["fleet"],
        )?;
        let agent_restarts = IntCounterVec::new(
            Opts::new(
                "hecaton_agent_restarts_total",
                "Agent exits noted by the reconciler",
            ),
            &["fleet", "crew", "agent"],
        )?;
        let hook_events = IntCounterVec::new(
            Opts::new("hecaton_hook_events_total", "Hook events accepted"),
            &["fleet", "crew", "agent", "event"],
        )?;
        let hook_handle_duration = HistogramVec::new(
            HistogramOpts::new("hecaton_hook_handle_duration_seconds", "Handler latency"),
            &["event"],
        )?;
        let hook_actions = IntCounterVec::new(
            Opts::new(
                "hecaton_hook_actions_total",
                "Actions executed for hook events",
            ),
            &["fleet", "crew", "agent", "action"],
        )?;
        for c in [
            Box::new(fleets.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(agents.clone()),
            Box::new(reconcile_duration.clone()),
            Box::new(reconcile_errors.clone()),
            Box::new(agent_restarts.clone()),
            Box::new(hook_events.clone()),
            Box::new(hook_handle_duration.clone()),
            Box::new(hook_actions.clone()),
        ] {
            registry.register(c)?;
        }
        // Instantiate hook_actions to ensure it appears in the output, even though
        // it's unused until Spec B.
        let _ = hook_actions.with_label_values(&["", "", "", ""]);
        Ok(Self {
            inner: Arc::new(Inner {
                registry,
                fleets,
                agents,
                reconcile_duration,
                reconcile_errors,
                agent_restarts,
                hook_events,
                hook_handle_duration,
                hook_actions,
            }),
        })
    }

    pub fn encode(&self) -> String {
        let mut buf = Vec::new();
        let _ = TextEncoder::new().encode(&self.inner.registry.gather(), &mut buf);
        String::from_utf8(buf).unwrap_or_default()
    }

    pub fn set_gauges(&self, records: &[FleetRecord]) {
        self.inner.fleets.reset();
        self.inner.agents.reset();
        for r in records {
            self.inner
                .fleets
                .with_label_values(&[&label(r.status.phase)])
                .inc();
            for (id, a) in &r.status.agents {
                let Ok(id) = id.parse::<AgentId>() else {
                    continue;
                };
                self.inner
                    .agents
                    .with_label_values(&[id.fleet.as_str(), id.crew.as_str(), &label(a.phase)])
                    .inc();
            }
        }
    }

    pub fn reconcile(&self, fleet: &str, secs: f64, ok: bool) {
        self.inner
            .reconcile_duration
            .with_label_values(&[fleet])
            .observe(secs);
        if !ok {
            self.inner
                .reconcile_errors
                .with_label_values(&[fleet])
                .inc();
        }
    }

    pub fn restart(&self, id: &AgentId) {
        self.inner
            .agent_restarts
            .with_label_values(&[id.fleet.as_str(), id.crew.as_str(), id.agent.as_str()])
            .inc();
    }

    pub fn hook_event(&self, id: &AgentId, event: &str, secs: f64) {
        self.inner
            .hook_events
            .with_label_values(&[
                id.fleet.as_str(),
                id.crew.as_str(),
                id.agent.as_str(),
                event,
            ])
            .inc();
        self.inner
            .hook_handle_duration
            .with_label_values(&[event])
            .observe(secs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hecaton_api::{AgentPhase, FleetPhase, FleetSpec};
    use hecaton_core::FleetRecord;
    use std::collections::BTreeMap;

    fn record(name: &str, phase: FleetPhase, agents: &[(&str, AgentPhase)]) -> FleetRecord {
        let mut r = FleetRecord::new(FleetSpec {
            name: name.into(),
            crews: BTreeMap::new(),
        });
        r.status.phase = phase;
        for (id, p) in agents {
            r.status.entry(id).phase = *p;
        }
        r
    }

    #[test]
    fn gauges_follow_the_snapshots_and_counters_accumulate() {
        let m = Metrics::new().unwrap();
        m.set_gauges(&[
            record(
                "f",
                FleetPhase::Ready,
                &[("f/c/a", AgentPhase::Ready), ("f/c/b", AgentPhase::Ready)],
            ),
            record("g", FleetPhase::Degraded, &[("g/d/x", AgentPhase::Dead)]),
        ]);
        let text = m.encode();
        assert!(text.contains("hecaton_fleets{phase=\"ready\"} 1"), "{text}");
        assert!(text.contains("hecaton_fleets{phase=\"degraded\"} 1"));
        assert!(text.contains("hecaton_agents{crew=\"c\",fleet=\"f\",phase=\"ready\"} 2"));
        assert!(text.contains("hecaton_agents{crew=\"d\",fleet=\"g\",phase=\"dead\"} 1"));
        // a fleet that disappears takes its gauges with it
        m.set_gauges(&[record(
            "f",
            FleetPhase::Ready,
            &[("f/c/a", AgentPhase::Ready)],
        )]);
        let text = m.encode();
        assert!(!text.contains("fleet=\"g\""));
        assert!(text.contains("hecaton_agents{crew=\"c\",fleet=\"f\",phase=\"ready\"} 1"));

        let id: AgentId = "f/c/a".parse().unwrap();
        m.reconcile("f", 0.25, true);
        m.reconcile("f", 0.5, false);
        m.restart(&id);
        m.hook_event(&id, "Notification", 0.001);
        m.hook_event(&id, "Notification", 0.002);
        let text = m.encode();
        assert!(text.contains("hecaton_reconcile_errors_total{fleet=\"f\"} 1"));
        assert!(text.contains("hecaton_reconcile_duration_seconds_count{fleet=\"f\"} 2"));
        assert!(
            text.contains("hecaton_agent_restarts_total{agent=\"a\",crew=\"c\",fleet=\"f\"} 1")
        );
        assert!(text.contains(
            "hecaton_hook_events_total{agent=\"a\",crew=\"c\",event=\"Notification\",fleet=\"f\"} 2"
        ));
        assert!(
            text.contains("hecaton_hook_handle_duration_seconds_count{event=\"Notification\"} 2")
        );
        for name in [
            "hecaton_fleets",
            "hecaton_agents",
            "hecaton_reconcile_duration_seconds",
            "hecaton_reconcile_errors_total",
            "hecaton_agent_restarts_total",
            "hecaton_hook_events_total",
            "hecaton_hook_handle_duration_seconds",
            "hecaton_hook_actions_total",
        ] {
            assert!(text.contains(&format!("# TYPE {name} ")), "{name} missing");
        }
    }
}
