//! Test doubles for the daemon: an in-memory `FleetStore` and a `Ports`
//! bundle over the `hecaton-core` fakes.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hecaton_api::Timestamp;
use hecaton_core::fakes::{FakeClock, FakeMaterializer, FakeRunner};
use hecaton_core::{FleetName, FleetRecord, FleetSecrets, FleetStore, ReconcilePolicy, StoreError};

use crate::actor::Ports;

#[derive(Default)]
pub struct MemoryStore {
    fleets: Mutex<BTreeMap<String, (FleetRecord, FleetSecrets)>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, (FleetRecord, FleetSecrets)>> {
        self.fleets.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn names(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }
    pub fn get(&self, name: &str) -> Option<(FleetRecord, FleetSecrets)> {
        self.lock().get(name).cloned()
    }
}

impl FleetStore for MemoryStore {
    fn load_all(&self) -> Result<Vec<(FleetRecord, FleetSecrets)>, StoreError> {
        Ok(self.lock().values().cloned().collect())
    }
    fn put(&self, record: &FleetRecord, secrets: &FleetSecrets) -> Result<(), StoreError> {
        self.lock()
            .insert(record.name().to_string(), (record.clone(), secrets.clone()));
        Ok(())
    }
    fn purge(&self, name: &FleetName) -> Result<(), StoreError> {
        self.lock().remove(name.as_str());
        Ok(())
    }
}

/// Fakes plus the `Ports` the actor and daemon take.
pub struct Harness {
    pub materializer: Arc<FakeMaterializer>,
    pub runner: Arc<FakeRunner>,
    pub clock: Arc<FakeClock>,
    pub store: Arc<MemoryStore>,
    pub ports: Arc<Ports>,
}

impl Harness {
    pub fn new(resync: Duration) -> Self {
        Self::with_policy(resync, ReconcilePolicy::default())
    }

    pub fn with_policy(resync: Duration, policy: ReconcilePolicy) -> Self {
        let materializer = Arc::new(FakeMaterializer::default());
        let runner = Arc::new(FakeRunner::default());
        let clock = Arc::new(FakeClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new());
        let ports = Arc::new(Ports {
            materializer: materializer.clone(),
            runner: runner.clone(),
            clock: clock.clone(),
            store: store.clone(),
            policy,
            hook_url: "http://127.0.0.1:1".to_string(),
            resync,
        });
        Self {
            materializer,
            runner,
            clock,
            store,
            ports,
        }
    }
}

/// A `PluginHostConfig` under a test directory: no `plugins.yaml` yet, so
/// the first sync is a no-op.
pub fn plugin_config_in(dir: &std::path::Path) -> crate::plugins::PluginHostConfig {
    crate::plugins::PluginHostConfig {
        plugins_file: dir.join("plugins.yaml"),
        install_root: dir.join("plugins"),
    }
}
