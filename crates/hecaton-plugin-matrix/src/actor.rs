//! The one task that owns every piece of mutable state (Spec G-10), the
//! bounded drop-oldest queue that feeds it (G-11), and the counters and
//! health cell it publishes through.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use hecaton_api::{HookEvent, OBSERVER_QUEUE};
use hecaton_plugin_sdk::metrics::{IntCounter, IntCounterVec, IntGauge};
use hecaton_plugin_sdk::{Metrics, SdkError};
use tokio::sync::Notify;

use crate::config::{AgentConfig, DaemonConfig};
use crate::matrix::Inbound;
use crate::render::PhaseChange;

/// Queue depth, the same as the daemon's own observer queues.
pub const QUEUE: usize = OBSERVER_QUEUE;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Configure(DaemonConfig),
    Activate { agent: String, config: AgentConfig },
    Deactivate { agent: String },
    Events(Vec<HookEvent>),
    Phases(Vec<PhaseChange>),
    Inbound(Inbound),
}

/// A bounded queue that drops its oldest entry rather than blocking its
/// producer: `observe` is a daemon-to-plugin HTTP call and must return.
pub struct Queue {
    inner: Mutex<VecDeque<Command>>,
    notify: Notify,
    dropped: IntCounter,
}

impl Queue {
    pub fn new(dropped: IntCounter) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(VecDeque::with_capacity(QUEUE)),
            notify: Notify::new(),
            dropped,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Command>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn push(&self, command: Command) {
        {
            let mut q = self.lock();
            if q.len() >= QUEUE {
                q.pop_front();
                self.dropped.inc();
            }
            q.push_back(command);
        }
        // `notify_one` stores a permit when nobody is waiting, so a pop
        // that arrives afterwards returns at once: no lost wakeups.
        self.notify.notify_one();
    }

    pub async fn pop(&self) -> Command {
        loop {
            if let Some(command) = self.lock().pop_front() {
                return command;
            }
            self.notify.notified().await;
        }
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The metric families of Spec G §10.
#[derive(Debug, Clone)]
pub struct Counters {
    pub messages_sent: IntCounterVec,
    pub events_dropped: IntCounter,
    pub inbound: IntCounterVec,
    pub rooms: IntGauge,
    pub threads_open: IntGauge,
    pub errors: IntCounterVec,
}

impl Counters {
    pub fn new(metrics: &Metrics) -> Result<Self, SdkError> {
        Ok(Self {
            messages_sent: metrics.int_counter_vec(
                "messages_sent_total",
                "Messages sent to Matrix, by kind",
                &["kind"],
            )?,
            events_dropped: metrics.int_counter(
                "events_dropped_total",
                "Commands dropped because the queue was full",
            )?,
            inbound: metrics.int_counter_vec(
                "inbound_total",
                "Matrix messages seen, by what became of them",
                &["outcome"],
            )?,
            rooms: metrics.int_gauge("rooms", "Crew rooms the plugin knows")?,
            threads_open: metrics
                .int_gauge("threads_open", "Agent sessions with an open thread")?,
            errors: metrics.int_counter_vec(
                "errors_total",
                "Matrix failures, by kind",
                &["kind"],
            )?,
        })
    }
}

/// What `Plugin::health` reports. The actor writes it; the plugin reads it.
#[derive(Debug, Clone, Default)]
pub struct Health(Arc<Mutex<Option<String>>>);

impl Health {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn ok(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    pub fn fail(&self, message: String) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
    }
    pub fn get(&self) -> Result<(), String> {
        match self.0.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            Some(m) => Err(m),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hecaton_plugin_sdk::Metrics;

    fn counters() -> Counters {
        Counters::new(&Metrics::new("matrix")).unwrap()
    }

    fn deactivate(agent: &str) -> Command {
        Command::Deactivate {
            agent: agent.to_string(),
        }
    }

    #[tokio::test]
    async fn the_queue_is_fifo_and_wakes_a_waiting_pop() {
        let c = counters();
        let q = Queue::new(c.events_dropped.clone());
        let popper = {
            let q = q.clone();
            tokio::spawn(async move { q.pop().await })
        };
        // Hand control to the scheduler so the popper actually runs, finds
        // the queue empty, and parks on `notified()` before the push below
        // exercises the wake path.
        tokio::task::yield_now().await;
        assert_eq!(
            q.len(),
            0,
            "the popper should have found nothing and parked"
        );
        q.push(deactivate("f/c/a"));
        assert_eq!(popper.await.unwrap(), deactivate("f/c/a"));

        q.push(deactivate("one"));
        q.push(deactivate("two"));
        assert_eq!(q.pop().await, deactivate("one"));
        assert_eq!(q.pop().await, deactivate("two"));
        assert_eq!(q.len(), 0);
    }

    #[tokio::test]
    async fn a_full_queue_drops_the_oldest_and_counts_it() {
        let c = counters();
        let q = Queue::new(c.events_dropped.clone());
        for i in 0..QUEUE {
            q.push(deactivate(&format!("a{i}")));
        }
        assert_eq!(q.len(), QUEUE);
        assert_eq!(c.events_dropped.get(), 0);

        q.push(deactivate("newest"));
        assert_eq!(q.len(), QUEUE, "capacity is held");
        assert_eq!(c.events_dropped.get(), 1);
        assert_eq!(
            q.pop().await,
            deactivate("a1"),
            "the oldest was dropped, not the newest"
        );
    }

    #[test]
    fn health_starts_ok_and_reports_the_last_failure_until_cleared() {
        let h = Health::new();
        assert_eq!(h.get(), Ok(()));
        h.fail("create room for f/c: no rights".into());
        assert_eq!(h.get(), Err("create room for f/c: no rights".into()));
        h.ok();
        assert_eq!(h.get(), Ok(()));
    }

    #[test]
    fn every_metric_family_carries_the_plugin_prefix() {
        let m = Metrics::new("matrix");
        let c = Counters::new(&m).unwrap();
        c.messages_sent.with_label_values(&["event"]).inc();
        c.inbound.with_label_values(&["routed"]).inc();
        c.errors.with_label_values(&["send"]).inc();
        c.rooms.set(2);
        c.threads_open.set(3);
        c.events_dropped.inc();
        let text = m.render().unwrap();
        for family in [
            "hecaton_plugin_matrix_messages_sent_total",
            "hecaton_plugin_matrix_events_dropped_total",
            "hecaton_plugin_matrix_inbound_total",
            "hecaton_plugin_matrix_rooms",
            "hecaton_plugin_matrix_threads_open",
            "hecaton_plugin_matrix_errors_total",
        ] {
            assert!(text.contains(family), "missing {family} in\n{text}");
        }
    }
}
