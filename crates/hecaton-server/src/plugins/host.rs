//! The plugin host (plugins spec §5.2): one ordinary fleet actor for the
//! reserved `hecaton` fleet, fed the synthetic spec from `plugins.yaml`,
//! with `hello` as its readiness event. The per-agent hook secret the actor
//! mints is the plugin's `HECATON_PLUGIN_TOKEN`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use hecaton_api::{
    AgentPhase, CredentialBundle, FleetSpec, HelloRequest, HelloResponse, PLUGIN_PROTOCOL,
    PluginStatus, SpecHash, SyncReport,
};
use hecaton_core::{
    AgentName, Clock, FleetRecord, FleetSecrets, Materializer, RESERVED_FLEET, ResolvedPlugin,
    plugin_fleet,
};
use tokio::sync::{Mutex, RwLock, oneshot};

use super::PluginError;
use super::config::{load_plugins_file, resolve_source};
use super::manifest::read_manifest;
use super::materializer::{NullStore, PluginMaterializer};
use super::package;
use crate::actor::{self, FleetHandle, Msg, Ports, READY_EVENT, Shared};
use crate::daemon::DaemonError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHostConfig {
    /// `$XDG_CONFIG_HOME/hecaton/plugins.yaml`.
    pub plugins_file: PathBuf,
    /// `$XDG_DATA_HOME/hecaton/plugins`: unpacked packages.
    pub install_root: PathBuf,
}

pub struct PluginHost {
    config: PluginHostConfig,
    materializer: Arc<PluginMaterializer>,
    handle: FleetHandle,
    clock: Arc<dyn Clock>,
    listen: RwLock<BTreeMap<AgentName, String>>,
    /// One sync at a time; a second `plugin sync` waits.
    syncing: Mutex<()>,
}

impl PluginHost {
    /// Spawns the `hecaton` fleet's actor with an empty spec. Nothing runs
    /// until the first `sync`.
    pub fn start(config: PluginHostConfig, agent_ports: &Ports, shared: Shared) -> Arc<Self> {
        let materializer = Arc::new(PluginMaterializer::new(agent_ports.materializer.clone()));
        let ports = Arc::new(Ports {
            materializer: materializer.clone(),
            runner: agent_ports.runner.clone(),
            clock: agent_ports.clock.clone(),
            store: Arc::new(NullStore),
            policy: agent_ports.policy.clone(),
            hook_url: agent_ports.hook_url.clone(),
            resync: agent_ports.resync,
        });
        let name = RESERVED_FLEET.parse().unwrap_or_else(|_| unreachable!());
        let record = FleetRecord::new(plugin_fleet(&[]).into());
        let handle = actor::spawn(name, record, FleetSecrets::default(), ports, shared, false);
        Arc::new(Self {
            config,
            materializer,
            handle,
            clock: agent_ports.clock.clone(),
            listen: RwLock::new(BTreeMap::new()),
            syncing: Mutex::new(()),
        })
    }

    pub fn handle(&self) -> &FleetHandle {
        &self.handle
    }

    pub fn record(&self) -> FleetRecord {
        self.handle.status.borrow().clone()
    }

    /// Reads `plugins.yaml`, installs or locates every package, validates
    /// every manifest. Blocking; the caller runs it in `spawn_blocking`.
    /// Every error carries the entry's path so the operator knows which
    /// plugin is wrong.
    pub fn resolve(config: &PluginHostConfig) -> Result<Vec<ResolvedPlugin>, PluginError> {
        let file = load_plugins_file(&config.plugins_file)?;
        let base = config
            .plugins_file
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let entry_error = |i: usize, field: &str, message: String| PluginError::Config {
            path: if field.is_empty() {
                format!("plugins[{i}]")
            } else {
                format!("plugins[{i}].{field}")
            },
            message,
        };
        let mut out = Vec::new();
        for (i, entry) in file.plugins.iter().enumerate() {
            let source = resolve_source(entry, &base)
                .map_err(|e| entry_error(i, "source", e.to_string()))?;
            let (dir, digest) = package::install(
                &entry.name,
                &source,
                entry.sha256.as_deref(),
                &config.install_root,
            )
            .map_err(|e| match e {
                PluginError::Digest { expected, got } => entry_error(
                    i,
                    "sha256",
                    format!("mismatch (expected {expected}, got {got})"),
                ),
                other => entry_error(i, "source", other.to_string()),
            })?;
            let manifest = read_manifest(&dir).map_err(|e| entry_error(i, "", e.to_string()))?;
            if manifest.name != entry.name {
                return Err(entry_error(
                    i,
                    "name",
                    format!("manifest says {:?}", manifest.name),
                ));
            }
            let name: AgentName = entry
                .name
                .parse()
                .map_err(|e: hecaton_core::NameError| entry_error(i, "name", e.to_string()))?;
            out.push(ResolvedPlugin {
                name,
                package: dir,
                manifest,
                config: entry.config.clone(),
                digest,
            });
        }
        Ok(out)
    }

    /// Reconciles the running set to `plugins.yaml`: resolves, swaps the
    /// materializer's map, applies the synthetic spec. The actor's pass
    /// then stops removed plugins, restarts changed ones and starts new
    /// ones. Nothing changes when `resolve` fails.
    pub async fn sync(&self) -> Result<SyncReport, PluginError> {
        let _guard = self.syncing.lock().await;
        let cfg = self.config.clone();
        let resolved = tokio::task::spawn_blocking(move || Self::resolve(&cfg))
            .await
            .map_err(|e| PluginError::Internal(format!("sync task panicked: {e}")))??;
        let before: BTreeMap<AgentName, SpecHash> = self
            .materializer
            .all()
            .iter()
            .map(|p| (p.name.clone(), p.hash()))
            .collect();
        let mut report = SyncReport::default();
        for p in &resolved {
            match before.get(&p.name) {
                Some(h) if *h == p.hash() => report.unchanged.push(p.name.to_string()),
                _ => report.installed.push(p.name.to_string()),
            }
        }
        for name in before.keys() {
            if !resolved.iter().any(|p| &p.name == name) {
                report.stopped.push(name.to_string());
            }
        }
        self.materializer.replace(resolved.clone());
        self.listen
            .write()
            .await
            .retain(|n, _| report.unchanged.iter().any(|u| u == n.as_str()));
        let spec: FleetSpec = plugin_fleet(&resolved).into();
        let (reply, rx) = oneshot::channel();
        self.handle
            .tx
            .send(Msg::Apply {
                spec,
                credentials: CredentialBundle::default(),
                reply,
            })
            .await
            .map_err(|_| PluginError::Internal("plugin actor is gone".into()))?;
        rx.await
            .map_err(|_| PluginError::Internal("plugin actor dropped the request".into()))?;
        Ok(report)
    }

    /// The plugin is up: record where it listens, hand back its config,
    /// and tell the actor — `hello` is the plugin's `SessionStart`.
    pub async fn hello(
        &self,
        name: &AgentName,
        req: HelloRequest,
    ) -> Result<HelloResponse, DaemonError> {
        let plugin = self
            .materializer
            .get(name)
            .ok_or(DaemonError::Unauthorized)?;
        if req.name != name.as_str() {
            return Err(DaemonError::Invalid(format!(
                "hello.name: {:?} does not match the token's plugin {:?}",
                req.name,
                name.as_str()
            )));
        }
        if req.protocol != PLUGIN_PROTOCOL {
            return Err(DaemonError::Invalid(format!(
                "hello.protocol: this daemon speaks protocol {PLUGIN_PROTOCOL}, got {}",
                req.protocol
            )));
        }
        let addr: SocketAddr = req.listen.parse().map_err(|_| {
            DaemonError::Invalid(format!("hello.listen: {:?} is not host:port", req.listen))
        })?;
        if !addr.ip().is_loopback() {
            return Err(DaemonError::Invalid(
                "hello.listen: must be a loopback address".into(),
            ));
        }
        self.listen
            .write()
            .await
            .insert(name.clone(), req.listen.clone());
        self.handle
            .tx
            .send(Msg::Event {
                agent: plugin.id(),
                name: READY_EVENT.into(),
                at: self.clock.now(),
            })
            .await
            .map_err(|_| DaemonError::Internal("plugin actor is gone".into()))?;
        Ok(HelloResponse {
            config: plugin.config.clone(),
        })
    }

    /// One row per declared plugin, sorted by name.
    pub async fn list(&self) -> Vec<PluginStatus> {
        let record = self.record();
        let listen = self.listen.read().await;
        self.materializer
            .all()
            .into_iter()
            .map(|p| {
                let st = record.status.agents.get(&p.id().to_string());
                PluginStatus {
                    name: p.name.to_string(),
                    version: p.manifest.version.clone(),
                    phase: st.map_or(AgentPhase::Pending, |s| s.phase),
                    listen: listen.get(&p.name).cloned(),
                    routes: p.manifest.routes,
                    message: st.map(|s| s.message.clone()).unwrap_or_default(),
                }
            })
            .collect()
    }

    /// `plugin remove --purge`: only for a plugin no longer declared.
    /// Deletes `plugins/<name>/` through the materializer and the installed
    /// packages under `install_root/<name>/`.
    pub async fn purge(&self, name: &AgentName) -> Result<(), PluginError> {
        if self.materializer.get(name).is_some() {
            return Err(PluginError::StillDeclared(name.to_string()));
        }
        let materializer = self.materializer.clone();
        let packages = self.config.install_root.join(name.as_str());
        let name = name.clone();
        tokio::task::spawn_blocking(move || {
            materializer
                .purge_plugin(&name)
                .map_err(|e| PluginError::Internal(e.to_string()))?;
            match std::fs::remove_dir_all(&packages) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(PluginError::io(&packages, e)),
            }
        })
        .await
        .map_err(|e| PluginError::Internal(format!("purge task panicked: {e}")))?
    }
}
