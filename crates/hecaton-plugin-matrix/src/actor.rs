//! The one task that owns every piece of mutable state (Spec G-10), the
//! bounded drop-oldest queue that feeds it (G-11), and the counters and
//! health cell it publishes through.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hecaton_api::{HookEvent, OBSERVER_QUEUE};
use hecaton_plugin_sdk::metrics::{IntCounter, IntCounterVec, IntGauge};
use hecaton_plugin_sdk::{Host, Metrics, SdkError};
use serde_json::Value;
use tokio::sync::Notify;

use crate::config::{AgentConfig, DaemonConfig};
use crate::matrix::{Inbound, MatrixError, MatrixPort};
use crate::render::{self, PhaseChange};
use crate::routing::{Maps, Thread, crew_of};

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

/// The one owner of every mutable piece of state (Spec G-10). Generic over
/// the port rather than holding a trait object, because the port's methods
/// return `impl Future` and so are not dyn-compatible (G-13).
pub struct Actor<M: MatrixPort> {
    host: Host,
    port: M,
    counters: Counters,
    health: Health,
    config: Option<DaemonConfig>,
    /// Commands that arrived before `Configure`. The daemon may `activate`
    /// before the SDK's `configure` returns, so nothing may be dropped.
    pending: Vec<Command>,
    agents: HashMap<String, AgentConfig>,
    maps: Maps,
}

impl<M: MatrixPort> Actor<M> {
    pub fn new(host: Host, port: M, counters: Counters, health: Health) -> Self {
        Self {
            host,
            port,
            counters,
            health,
            config: None,
            pending: Vec::new(),
            agents: HashMap::new(),
            maps: Maps::new(),
        }
    }

    /// Replaces the health cell. Only the wiring and its tests use this.
    pub fn set_health(&mut self, health: Health) {
        self.health = health;
    }

    /// Restores the room and thread maps from KV, so a restart resumes.
    pub async fn load(&mut self) {
        match Maps::load(&self.host).await {
            Ok(maps) => self.maps = maps,
            Err(e) => tracing::warn!("matrix: loading maps: {e}"),
        }
        self.publish_gauges();
    }

    pub async fn run(mut self, queue: Arc<Queue>) {
        loop {
            let command = queue.pop().await;
            self.handle(command).await;
        }
    }

    pub async fn handle(&mut self, command: Command) {
        if let Command::Configure(config) = command {
            self.config = Some(config);
            for buffered in std::mem::take(&mut self.pending) {
                Box::pin(self.handle(buffered)).await;
            }
            return;
        }
        if self.config.is_none() {
            self.pending.push(command);
            return;
        }
        match command {
            Command::Configure(_) => unreachable!("handled above"),
            Command::Activate { agent, config } => {
                self.agents.insert(agent, config);
            }
            Command::Deactivate { agent } => {
                self.agents.remove(&agent);
                if let Err(e) = self.maps.forget(&self.host, &agent).await {
                    tracing::warn!("matrix: forgetting {agent}: {e}");
                }
                self.publish_gauges();
            }
            Command::Events(events) => {
                for event in events {
                    self.on_event(event).await;
                }
            }
            Command::Phases(changes) => {
                for change in changes {
                    self.on_phase(change).await;
                }
            }
            Command::Inbound(message) => self.on_inbound(message).await,
        }
    }

    fn publish_gauges(&self) {
        self.counters.rooms.set(self.maps.rooms_len() as i64);
        self.counters
            .threads_open
            .set(self.maps.open_threads() as i64);
    }

    /// The crew's room: known, then pinned, then created. `None` means the
    /// homeserver refused, which is reported and retried on the next event.
    async fn room_for(&mut self, agent: &str) -> Option<String> {
        let crew = crew_of(agent)?.to_string();
        if let Some(room) = self.maps.room(&crew) {
            return Some(room.to_string());
        }
        let (pinned, invite) = {
            let config = self.config.as_ref()?;
            (config.rooms.get(&crew).cloned(), config.invite.clone())
        };
        let room = match pinned {
            Some(room) => room,
            None => {
                let name = format!("hecaton {crew}");
                match retry_once(|| self.port.create_room(&name, &invite)).await {
                    Ok(room) => room,
                    Err(e) => {
                        self.counters
                            .errors
                            .with_label_values(&["create_room"])
                            .inc();
                        self.health.fail(format!("create room for {crew}: {e}"));
                        tracing::warn!("matrix: create room for {crew}: {e}");
                        return None;
                    }
                }
            }
        };
        if let Err(e) = self.maps.set_room(&self.host, &crew, &room).await {
            tracing::warn!("matrix: storing room for {crew}: {e}");
        }
        self.health.ok();
        self.publish_gauges();
        Some(room)
    }

    /// One send. `None` means the message did not land, and the caller must
    /// not go on to post anything that depends on it.
    async fn send(
        &self,
        room: &str,
        thread_root: Option<&str>,
        body: &str,
        kind: &str,
    ) -> Option<String> {
        match retry_once(|| self.port.send(room, thread_root, body)).await {
            Ok(id) => {
                self.counters.messages_sent.with_label_values(&[kind]).inc();
                Some(id)
            }
            Err(e) => {
                self.counters.errors.with_label_values(&["send"]).inc();
                tracing::warn!("matrix: send to {room}: {e}");
                None
            }
        }
    }

    async fn on_event(&mut self, event: HookEvent) {
        let Some(config) = self.agents.get(&event.agent).cloned() else {
            return;
        };
        if !config.enabled {
            return;
        }
        let Some(room) = self.room_for(&event.agent).await else {
            return;
        };
        let session = event.session_id.clone().unwrap_or_default();

        // A thread is opened for a new session id, and for an agent we
        // have no thread for at all: the plugin may have started mid
        // session, and an event must never be dropped for want of a root.
        let opening = match self.maps.thread(&event.agent) {
            None => true,
            Some(thread) => !session.is_empty() && thread.session_id != session,
        };
        if opening {
            let source = event
                .payload
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("already running");
            // `thread_root` interpolates the hook payload's `source`, so
            // like every other body it goes through the 4000-character cut.
            let body = render::truncate(&render::thread_root(&event.agent, &session, source));
            let Some(root) = self.send(&room, None, &body, "root").await else {
                return;
            };
            let thread = Thread {
                session_id: session,
                root,
                room: room.clone(),
                closed: false,
            };
            if let Err(e) = self.maps.set_thread(&self.host, &event.agent, thread).await {
                tracing::warn!("matrix: storing thread for {}: {e}", event.agent);
            }
            self.publish_gauges();
            // The root already says the session started.
            if event.name == "SessionStart" {
                return;
            }
        } else if let Some(mut thread) =
            self.maps.thread(&event.agent).filter(|t| t.closed).cloned()
        {
            // The session is emitting again after a `SessionEnd` — an
            // operator resuming it — so its thread is alive whatever the
            // record says. Left closed, the user would watch the agent
            // work in a thread where inbound routing refuses every reply
            // as stale (Spec G-12), and `threads_open` would undercount
            // from here on. A failed write leaves only the flag stale,
            // the root being unchanged, so this posts anyway rather than
            // dropping the event.
            thread.closed = false;
            if let Err(e) = self.maps.set_thread(&self.host, &event.agent, thread).await {
                tracing::warn!("matrix: reopening thread for {}: {e}", event.agent);
            }
            self.publish_gauges();
        }

        if !config.wants(&event.name) {
            return;
        }
        let Some(root) = self.maps.thread(&event.agent).map(|t| t.root.clone()) else {
            return;
        };
        let body = render::event_message(&event);
        self.send(&room, Some(&root), &body, "event").await;

        if event.name == "SessionEnd" {
            if let Err(e) = self.maps.close_thread(&self.host, &event.agent).await {
                tracing::warn!("matrix: closing thread for {}: {e}", event.agent);
            }
            self.publish_gauges();
        }
    }

    async fn on_phase(&mut self, change: PhaseChange) {
        let Some(config) = self.agents.get(&change.agent).cloned() else {
            return;
        };
        if !config.enabled || !config.phases {
            return;
        }
        let Some(room) = self.room_for(&change.agent).await else {
            return;
        };
        let root = self.maps.thread(&change.agent).map(|t| t.root.clone());
        let body = render::phase_message(&change);
        self.send(&room, root.as_deref(), &body, "phase").await;
    }

    /// Task 10.
    async fn on_inbound(&mut self, _message: Inbound) {}
}

/// One port call, retried once on a rate limit after the delay the
/// homeserver itself asked for. Room creation is rate limited as readily as
/// a message, so both go through here.
async fn retry_once<T, F, Fut>(call: F) -> Result<T, MatrixError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, MatrixError>>,
{
    let attempt = call().await;
    if let Err(MatrixError::RateLimited { retry_after_ms }) = attempt {
        tokio::time::sleep(Duration::from_millis(retry_after_ms)).await;
        return call().await;
    }
    attempt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::fake::{Call, FakePort};
    use hecaton_plugin_sdk::testing::{FakeHost, event};
    use hecaton_plugin_sdk::{Host, Metrics};
    use serde_json::json;

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

    fn daemon_config() -> DaemonConfig {
        crate::config::parse_daemon(&json!({
            "homeserver": "https://h",
            "userId": "@hecaton:example.org",
            "password": "pw",
            "invite": ["@rahul:example.org"]
        }))
        .unwrap()
    }

    fn agent_config(events: &[&str]) -> AgentConfig {
        crate::config::parse_agent(&json!({ "events": events })).unwrap()
    }

    fn started(agent: &str, session: &str, source: &str) -> HookEvent {
        let mut e = event(agent, "SessionStart", json!({ "source": source }));
        e.session_id = Some(session.to_string());
        e
    }

    fn during(agent: &str, session: &str, name: &str, payload: serde_json::Value) -> HookEvent {
        let mut e = event(agent, name, payload);
        e.session_id = Some(session.to_string());
        e
    }

    async fn actor() -> (FakeHost, FakePort, Actor<FakePort>) {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("matrix", std::path::Path::new("scratch"))).unwrap();
        let port = FakePort::new("@hecaton:example.org");
        let a = Actor::new(host, port.clone(), counters(), Health::new());
        (fake, port, a)
    }

    /// The event id `FakePort` returned for the root send recorded at
    /// `index`: it mints `$evt<n>:fake` on its nth successful call, so the
    /// id is knowable from the call sequence alone. Asserting a child
    /// against *this* is the point — a child threaded under the session id,
    /// the room id or any other non-empty string satisfies `is_some()` but
    /// lands nowhere a Matrix client would show it.
    fn minted_root(calls: &[Call], index: usize) -> String {
        assert!(
            matches!(
                calls.get(index),
                Some(Call::Send {
                    thread_root: None,
                    ..
                })
            ),
            "call {index} is not a thread root: {calls:?}"
        );
        format!("$evt{}:fake", index + 1)
    }

    fn sends(calls: &[Call]) -> Vec<(Option<String>, String)> {
        calls
            .iter()
            .filter_map(|c| match c {
                Call::Send {
                    thread_root, body, ..
                } => Some((thread_root.clone(), body.clone())),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn commands_before_configure_are_buffered_and_replayed_in_order() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        assert!(port.calls().is_empty(), "nothing before configure");

        a.handle(Command::Configure(daemon_config())).await;
        let calls = port.calls();
        assert!(
            matches!(calls.first(), Some(Call::CreateRoom { .. })),
            "the room comes first: {calls:?}"
        );
        assert_eq!(sends(&calls).len(), 1, "then the thread root");
        assert_eq!(sends(&calls)[0].0, None, "the root is not a thread reply");
    }

    #[tokio::test]
    async fn two_agents_in_one_crew_share_a_single_room() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        for name in ["alice", "bob"] {
            a.handle(Command::Activate {
                agent: format!("f/c/{name}"),
                config: agent_config(&["Notification"]),
            })
            .await;
            a.handle(Command::Events(vec![started(
                &format!("f/c/{name}"),
                "s1",
                "startup",
            )]))
            .await;
        }
        let rooms = port
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::CreateRoom { .. }))
            .count();
        assert_eq!(rooms, 1, "one room per crew");
        assert_eq!(sends(&port.calls()).len(), 2, "one root per agent");
    }

    #[tokio::test]
    async fn a_root_exists_before_any_child_and_children_are_thread_replies() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        a.handle(Command::Events(vec![
            started("f/c/alice", "s1", "startup"),
            during(
                "f/c/alice",
                "s1",
                "Notification",
                json!({ "message": "needs permission" }),
            ),
        ]))
        .await;

        let calls = port.calls();
        // Call 0 is the room creation, so the root is call 1.
        let root = minted_root(&calls, 1);
        let s = sends(&calls);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].0, None, "root first");
        assert!(s[0].1.contains("session"), "{}", s[0].1);
        assert_eq!(
            s[1].0,
            Some(root),
            "the child hangs off the id the root send returned"
        );
        assert!(s[1].1.contains("needs permission"), "{}", s[1].1);
    }

    #[tokio::test]
    async fn a_same_id_session_start_reuses_the_thread_and_a_new_id_opens_another() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        // Call 0 is the room creation, so the root is call 1.
        let root = minted_root(&port.take_calls(), 1);

        a.handle(Command::Events(vec![started("f/c/alice", "s1", "compact")]))
            .await;
        let s = sends(&port.take_calls());
        assert_eq!(s.len(), 1);
        assert_eq!(
            s[0].0,
            Some(root),
            "a compaction posts inside the very thread the root opened"
        );
        assert!(s[0].1.contains("restarted"), "{}", s[0].1);

        a.handle(Command::Events(vec![started("f/c/alice", "s2", "clear")]))
            .await;
        let s = sends(&port.take_calls());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, None, "a new session id opens a new root");
    }

    #[tokio::test]
    async fn session_end_closes_the_thread_and_the_filter_drops_unwanted_events() {
        let (fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        a.handle(Command::Events(vec![
            started("f/c/alice", "s1", "startup"),
            during(
                "f/c/alice",
                "s1",
                "PreToolUse",
                json!({ "tool_name": "Bash" }),
            ),
            during(
                "f/c/alice",
                "s1",
                "SessionEnd",
                json!({ "reason": "clear" }),
            ),
        ]))
        .await;

        let bodies: Vec<String> = sends(&port.calls()).into_iter().map(|(_, b)| b).collect();
        assert_eq!(bodies.len(), 2, "PreToolUse is filtered out: {bodies:?}");
        assert!(bodies[1].contains("session ended"), "{}", bodies[1]);

        let stored = fake.kv_json("thread/f/c/alice").unwrap();
        assert_eq!(stored["closed"], true);
    }

    #[tokio::test]
    async fn a_session_start_after_a_session_end_reopens_the_same_thread() {
        let (fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        a.handle(Command::Events(vec![
            started("f/c/alice", "s1", "startup"),
            during(
                "f/c/alice",
                "s1",
                "SessionEnd",
                json!({ "reason": "clear" }),
            ),
        ]))
        .await;
        // Call 0 is the room creation, so the root is call 1.
        let root = minted_root(&port.take_calls(), 1);
        assert_eq!(fake.kv_json("thread/f/c/alice").unwrap()["closed"], true);

        // The operator resumes that very session.
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "resume")]))
            .await;

        let s = sends(&port.take_calls());
        assert_eq!(s.len(), 1, "the same session opens no second root");
        assert_eq!(s[0].0, Some(root), "the restart posts inside that thread");
        let stored = fake.kv_json("thread/f/c/alice").unwrap();
        assert_eq!(
            stored["closed"], false,
            "and the thread is open again, so a reply in it still routes"
        );

        // A later event posts in the same thread and leaves it open.
        a.handle(Command::Events(vec![during(
            "f/c/alice",
            "s1",
            "Notification",
            json!({ "message": "needs permission" }),
        )]))
        .await;
        assert_eq!(fake.kv_json("thread/f/c/alice").unwrap()["closed"], false);
    }

    #[tokio::test]
    async fn a_disabled_agent_and_an_unknown_agent_post_nothing() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: crate::config::parse_agent(&json!({ "enabled": false })).unwrap(),
        })
        .await;
        a.handle(Command::Events(vec![
            started("f/c/alice", "s1", "startup"),
            started("f/c/ghost", "s1", "startup"),
        ]))
        .await;
        assert!(port.calls().is_empty(), "{:?}", port.calls());
    }

    #[tokio::test]
    async fn a_pinned_room_is_used_instead_of_creating_one() {
        let (_fake, port, mut a) = actor().await;
        let mut cfg = daemon_config();
        cfg.rooms.insert("f/c".into(), "!pinned:example.org".into());
        a.handle(Command::Configure(cfg)).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&[]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;

        assert!(
            !port
                .calls()
                .iter()
                .any(|c| matches!(c, Call::CreateRoom { .. })),
            "no room was created"
        );
        assert!(matches!(
            port.calls().first(),
            Some(Call::Send { room, .. }) if room == "!pinned:example.org"
        ));
    }

    #[tokio::test]
    async fn a_failed_room_creation_is_reported_and_retried_on_the_next_event() {
        let (_fake, port, mut a) = actor().await;
        let health = Health::new();
        a.set_health(health.clone());
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;

        port.fail_next(crate::matrix::MatrixError::Other("no rights".into()));
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        assert!(health.get().is_err(), "the failure is visible");
        assert!(sends(&port.calls()).is_empty());

        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        assert_eq!(sends(&port.calls()).len(), 1, "the next event retries");
        assert_eq!(health.get(), Ok(()), "and clears the failure");
    }

    #[tokio::test]
    async fn a_failed_root_send_posts_nothing_and_stores_no_thread() {
        let (fake, port, mut a) = actor().await;
        let mut cfg = daemon_config();
        // Pinning the room means the call that fails below is the thread
        // root's send and not the room creation.
        cfg.rooms.insert("f/c".into(), "!pinned:example.org".into());
        a.handle(Command::Configure(cfg)).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;

        port.fail_next(crate::matrix::MatrixError::Other("no rights".into()));
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;

        assert!(sends(&port.calls()).is_empty(), "{:?}", port.calls());
        assert!(
            !fake.kv().contains_key("thread/f/c/alice"),
            "a thread whose root never landed is not a thread"
        );
    }

    #[tokio::test]
    async fn a_child_in_the_same_batch_as_a_failed_root_is_never_posted_rootless() {
        let (fake, port, mut a) = actor().await;
        let mut cfg = daemon_config();
        cfg.rooms.insert("f/c".into(), "!pinned:example.org".into());
        a.handle(Command::Configure(cfg)).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;

        port.fail_next(crate::matrix::MatrixError::Other("no rights".into()));
        a.handle(Command::Events(vec![
            started("f/c/alice", "s1", "startup"),
            during(
                "f/c/alice",
                "s1",
                "Notification",
                json!({ "message": "needs permission" }),
            ),
        ]))
        .await;

        // The `SessionStart`'s root never landed and nothing was stored for
        // it, so the notification had to open a root of its own before it
        // could post: had the actor kept a thread despite the failed send,
        // the first thing recorded here would be a child under a root that
        // does not exist in the room.
        let calls = port.calls();
        let root = minted_root(&calls, 0);
        let s = sends(&calls);
        assert_eq!(s.len(), 2, "a root, then the notification: {calls:?}");
        assert_eq!(s[1].0, Some(root.clone()), "under the root that landed");
        assert!(s[1].1.contains("needs permission"), "{}", s[1].1);
        assert_eq!(
            fake.kv_json("thread/f/c/alice").unwrap()["root"],
            json!(root),
            "and the stored root is the one that landed, not the one that failed"
        );
    }

    #[tokio::test]
    async fn a_rate_limited_send_is_retried_once() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        // The room and the thread root have to exist first, or the call the
        // rate limit lands on would be `create_room`, not a send.
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        // Call 0 is the room creation, so the root is call 1.
        let root = minted_root(&port.take_calls(), 1);

        port.fail_next(crate::matrix::MatrixError::RateLimited { retry_after_ms: 1 });
        a.handle(Command::Events(vec![during(
            "f/c/alice",
            "s1",
            "Notification",
            json!({ "message": "needs permission" }),
        )]))
        .await;

        let calls = port.take_calls();
        assert!(
            calls.iter().all(|c| matches!(c, Call::Send { .. })),
            "the rate limit landed on a send, not a room creation: {calls:?}"
        );
        let s = sends(&calls);
        assert_eq!(s.len(), 1, "the retry got through");
        assert_eq!(s[0].0, Some(root), "and it is still the thread reply");
        assert!(s[0].1.contains("needs permission"), "{}", s[0].1);
    }

    #[tokio::test]
    async fn a_rate_limited_room_creation_is_retried_once() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&[]),
        })
        .await;
        // Nothing has been posted yet, so the first port call — and the one
        // the rate limit lands on — is the room creation.
        port.fail_next(crate::matrix::MatrixError::RateLimited { retry_after_ms: 1 });
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;

        let calls = port.calls();
        assert!(
            matches!(calls.first(), Some(Call::CreateRoom { .. })),
            "the retry created the room: {calls:?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|c| matches!(c, Call::CreateRoom { .. }))
                .count(),
            1,
            "and only one room came of it"
        );
        assert_eq!(sends(&calls).len(), 1, "the thread root follows it");
    }

    #[tokio::test]
    async fn a_phase_change_posts_into_the_thread_when_there_is_one() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&[]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        // Call 0 is the room creation, so the root is call 1.
        let root = minted_root(&port.take_calls(), 1);
        a.handle(Command::Phases(vec![PhaseChange {
            agent: "f/c/alice".into(),
            from: hecaton_api::AgentPhase::Ready,
            to: hecaton_api::AgentPhase::Dead,
            message: "window gone".into(),
        }]))
        .await;
        let s = sends(&port.calls());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, Some(root), "in the agent's own thread");
        assert!(s[0].1.contains("window gone"), "{}", s[0].1);
    }

    #[tokio::test]
    async fn deactivate_forgets_the_agent_and_its_thread() {
        let (fake, _port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&[]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        assert!(fake.kv().contains_key("thread/f/c/alice"));
        a.handle(deactivate("f/c/alice")).await;
        assert!(!fake.kv().contains_key("thread/f/c/alice"));
    }
}
