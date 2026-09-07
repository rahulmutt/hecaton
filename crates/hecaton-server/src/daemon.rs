//! The registry (Phase 3 spec §3.1): fleet name → actor handle, the shared
//! secret index, and the request-side logic the API calls into.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use hecaton_api::{
    CredentialBundle, FleetSpec, FleetSummary, HelloRequest, HelloResponse, HookEvent, SyncReport,
};
use hecaton_core::{
    AgentId, AgentName, EventHandler, Fleet, FleetName, FleetRecord, FleetSecrets, Keep, Outcome,
    is_reserved_fleet, plugin_id,
};
use tokio::sync::{RwLock, mpsc, oneshot};

use crate::actor::{self, FleetHandle, Msg, Ports, Shared};
use crate::auth::constant_time_eq;
use crate::hooks::ParsedEvent;
use crate::metrics::Metrics;
use crate::plugins::{PluginError, PluginHost, PluginHostConfig};

pub struct Daemon {
    fleets: RwLock<BTreeMap<FleetName, FleetHandle>>,
    ports: Arc<Ports>,
    shared: Shared,
    handler: Arc<dyn EventHandler>,
    token: String,
    plugins: Arc<PluginHost>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DaemonError {
    #[error("fleet not found")]
    NotFound,
    #[error("fleet exists; use `hecaton update`, or `hecaton down` first")]
    Conflict,
    #[error("{0}")]
    Invalid(String),
    #[error("unknown agent or bad secret")]
    Unauthorized,
    #[error("{0}")]
    Internal(String),
}

impl Daemon {
    /// Spawns one actor per stored fleet (each reconciles once) and the
    /// listener that drops purged fleets. Needs a tokio runtime.
    pub fn start(
        ports: Ports,
        handler: Arc<dyn EventHandler>,
        metrics: Metrics,
        token: String,
        existing: Vec<(FleetRecord, FleetSecrets)>,
        plugin_config: PluginHostConfig,
    ) -> Arc<Self> {
        let (shared, purged) = actor::shared(metrics);
        let plugins = PluginHost::start(plugin_config, &ports, shared.clone());
        let ports = Arc::new(ports);
        let mut fleets = BTreeMap::new();
        for (record, secrets) in existing {
            match FleetName::try_from(record.spec.name.clone()) {
                Ok(name) => {
                    let h = actor::spawn(
                        name.clone(),
                        record,
                        secrets,
                        ports.clone(),
                        shared.clone(),
                        true,
                    );
                    fleets.insert(name, h);
                }
                Err(e) => tracing::error!("skipping a stored fleet with an invalid name: {e}"),
            }
        }
        let daemon = Arc::new(Self {
            fleets: RwLock::new(fleets),
            ports,
            shared,
            handler,
            token,
            plugins,
        });
        tokio::spawn(Self::forget_purged(Arc::downgrade(&daemon), purged));
        daemon
    }

    async fn forget_purged(daemon: std::sync::Weak<Self>, mut purged: mpsc::Receiver<FleetName>) {
        while let Some(name) = purged.recv().await {
            let Some(d) = daemon.upgrade() else {
                return;
            };
            d.fleets.write().await.remove(&name);
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn metrics(&self) -> &Metrics {
        &self.shared.metrics
    }

    pub fn plugins(&self) -> &Arc<PluginHost> {
        &self.plugins
    }

    /// Reconciles the plugin set to `plugins.yaml`; `serve` calls it once
    /// at start and fails fast on an error, `plugin sync` on demand.
    pub async fn sync_plugins(&self) -> Result<SyncReport, PluginError> {
        self.plugins.sync().await
    }

    /// `hello` authenticates with the plugin's token — the hook secret the
    /// actor minted for `hecaton/plugins/<name>` — and is otherwise the
    /// plugin's `SessionStart`.
    pub async fn plugin_hello(
        &self,
        name: &AgentName,
        token: &str,
        req: HelloRequest,
    ) -> Result<HelloResponse, DaemonError> {
        if !self.verify_secret(&plugin_id(name), token).await {
            return Err(DaemonError::Unauthorized);
        }
        self.plugins.hello(name, req).await
    }

    /// The `hecaton` fleet belongs to the plugin host: readable, never
    /// written through the fleet API.
    fn reject_reserved(name: &FleetName) -> Result<(), DaemonError> {
        if is_reserved_fleet(name.as_str()) {
            return Err(DaemonError::Invalid(format!(
                "name: {:?} is reserved for the daemon's plugins",
                name.as_str()
            )));
        }
        Ok(())
    }

    /// `POST` (`replace == false`): 409 unless the fleet is absent or
    /// settled `Down`. `PUT` (`replace == true`): 404 when absent.
    pub async fn apply(
        &self,
        name: &FleetName,
        spec: FleetSpec,
        credentials: CredentialBundle,
        replace: bool,
    ) -> Result<FleetRecord, DaemonError> {
        Self::reject_reserved(name)?;
        if spec.name != name.as_str() {
            return Err(DaemonError::Invalid(format!(
                "spec.name {:?} does not match the fleet {name}",
                spec.name
            )));
        }
        Fleet::try_from(spec.clone()).map_err(|e| DaemonError::Invalid(e.to_string()))?;
        let handle = {
            let mut fleets = self.fleets.write().await;
            match fleets.get(name) {
                Some(h) => {
                    if !replace && !h.status.borrow().is_down() {
                        return Err(DaemonError::Conflict);
                    }
                    h.clone()
                }
                None => {
                    if replace {
                        return Err(DaemonError::NotFound);
                    }
                    let h = actor::spawn(
                        name.clone(),
                        FleetRecord::new(spec.clone()),
                        FleetSecrets::default(),
                        self.ports.clone(),
                        self.shared.clone(),
                        false,
                    );
                    fleets.insert(name.clone(), h.clone());
                    h
                }
            }
        };
        let (reply, rx) = oneshot::channel();
        handle
            .tx
            .send(Msg::Apply {
                spec,
                credentials,
                reply,
            })
            .await
            .map_err(|_| DaemonError::Internal("fleet task is gone".into()))?;
        rx.await
            .map_err(|_| DaemonError::Internal("fleet task dropped the request".into()))
    }

    pub async fn down(
        &self,
        name: &FleetName,
        keep: Keep,
        purge: bool,
    ) -> Result<FleetRecord, DaemonError> {
        Self::reject_reserved(name)?;
        let handle = self
            .fleets
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or(DaemonError::NotFound)?;
        let (reply, rx) = oneshot::channel();
        handle
            .tx
            .send(Msg::Down { keep, purge, reply })
            .await
            .map_err(|_| DaemonError::Internal("fleet task is gone".into()))?;
        rx.await
            .map_err(|_| DaemonError::Internal("fleet task dropped the request".into()))
    }

    pub async fn get(&self, name: &FleetName) -> Option<FleetRecord> {
        if is_reserved_fleet(name.as_str()) {
            return Some(self.plugins.record());
        }
        self.fleets
            .read()
            .await
            .get(name)
            .map(|h| h.status.borrow().clone())
    }

    /// Every record `/metrics` gauges, the plugin fleet included.
    pub async fn snapshots(&self) -> Vec<FleetRecord> {
        let mut out: Vec<FleetRecord> = self
            .fleets
            .read()
            .await
            .values()
            .map(|h| h.status.borrow().clone())
            .collect();
        out.push(self.plugins.record());
        out
    }

    /// User fleets only: the plugin fleet is not a fleet row.
    pub async fn list(&self) -> Vec<FleetSummary> {
        self.fleets
            .read()
            .await
            .values()
            .map(|h| h.status.borrow().summary())
            .collect()
    }

    pub async fn hook_secret(&self, agent: &AgentId) -> Option<String> {
        self.shared.hook_secrets.read().await.get(agent).cloned()
    }

    /// Constant-time compare against the agent's current secret. `false`
    /// for an unknown agent and a wrong secret alike. Ingress calls this
    /// *before* the rate limiter (spec §3.5): an unauthenticated caller
    /// must never be able to touch — let alone drain or grow — another
    /// agent's bucket.
    pub async fn verify_secret(&self, agent: &AgentId, secret: &str) -> bool {
        match self.hook_secret(agent).await {
            Some(s) => constant_time_eq(s.as_bytes(), secret.as_bytes()),
            None => false,
        }
    }

    /// Authenticates, forwards the event to the fleet, runs the handler.
    /// Unknown agent and bad secret are the same error on purpose.
    pub async fn event(
        &self,
        agent: &AgentId,
        secret: &str,
        event: ParsedEvent,
    ) -> Result<Outcome, DaemonError> {
        let expected = self.hook_secret(agent).await;
        match expected {
            Some(s) if constant_time_eq(s.as_bytes(), secret.as_bytes()) => {}
            _ => return Err(DaemonError::Unauthorized),
        }
        let handle = self
            .fleets
            .read()
            .await
            .get(&agent.fleet)
            .cloned()
            .ok_or(DaemonError::Unauthorized)?;
        let started = Instant::now();
        let at = self.ports.clock.now();
        handle
            .tx
            .send(Msg::Event {
                agent: agent.clone(),
                name: event.name.clone(),
                at,
            })
            .await
            .map_err(|_| DaemonError::Internal("fleet task is gone".into()))?;
        let hook_event = HookEvent {
            agent: agent.to_string(),
            name: event.name,
            session_id: event.session_id,
            received_at: at,
            payload: event.payload,
        };
        tracing::debug!(agent = %agent, event = %hook_event.name, payload = %hook_event.payload, "hook event");
        let outcome = self.handler.handle(&hook_event);
        self.shared
            .metrics
            .hook_event(agent, &hook_event.name, started.elapsed().as_secs_f64());
        Ok(outcome)
    }
}
