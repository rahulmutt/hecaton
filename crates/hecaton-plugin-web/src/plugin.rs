//! The `Plugin` impl (plugins spec §18.5): a per-agent `enabled` flag, the
//! cache `fleets/watch` feeds, and the two terminal metrics. The routes
//! (`crate::routes`) share `Shared` with it.

use std::sync::Arc;

use hecaton_plugin_sdk::metrics::{IntCounter, IntGauge};
use hecaton_plugin_sdk::{Host, Metrics, Plugin, SdkError};
use serde_json::Value;

use crate::config::parse;
use crate::state::Cache;

/// What the plugin and its routes both hold.
pub struct Shared {
    pub host: Host,
    pub cache: Cache,
    pub terminals_open: IntGauge,
    pub terminals_total: IntCounter,
}

pub struct WebPlugin {
    shared: Arc<Shared>,
    metrics: Metrics,
}

impl std::fmt::Debug for WebPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebPlugin")
            .field("host", &self.shared.host)
            .finish()
    }
}

impl WebPlugin {
    pub fn new(host: Host) -> Result<Self, SdkError> {
        let metrics = Metrics::new(&host.env().name);
        let terminals_open = metrics.int_gauge("terminals_open", "Browser terminals open now")?;
        let terminals_total =
            metrics.int_counter("terminals_total", "Browser terminals opened since start")?;
        Ok(Self {
            shared: Arc::new(Shared {
                host,
                cache: Cache::new(),
                terminals_open,
                terminals_total,
            }),
            metrics,
        })
    }

    pub fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    /// Feeds the cache from `fleets/watch` for as long as the task lives.
    /// Started before `serve`: the route needs the token and the `fleets`
    /// capability, not readiness, and the first frame is the current list.
    pub fn start_watch(&self) -> tokio::task::JoinHandle<()> {
        let shared = self.shared.clone();
        tokio::spawn(async move {
            let mut watch = shared.host.watch_fleets();
            loop {
                shared.cache.set_fleets(watch.next().await);
            }
        })
    }
}

impl Plugin for WebPlugin {
    async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        let cfg = parse(&config).map_err(|e| e.to_string())?;
        self.shared.cache.set_enabled(agent, cfg.enabled);
        eprintln!(
            "web: {agent} {}",
            if cfg.enabled { "listed" } else { "hidden" }
        );
        Ok(())
    }

    async fn deactivate(&self, agent: &str) {
        self.shared.cache.remove(agent);
    }

    fn metrics(&self) -> Option<&Metrics> {
        Some(&self.metrics)
    }

    fn routes(&self) -> Option<axum::Router> {
        Some(crate::routes::router(self.shared.clone()))
    }
}
