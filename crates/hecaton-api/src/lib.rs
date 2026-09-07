//! Wire types shared by the hecaton CLI and daemon (spec §3).
//!
//! This crate is a leaf: serde DTOs only, no logic beyond defaults and
//! secret-redacting `Debug` impls.

/// The `apiVersion` every fleet file and request declares.
pub const API_VERSION: &str = "hecaton/v1";

pub mod credentials;
pub mod fleet;
pub mod hook;
pub mod plugin;
pub mod protocol;
pub mod record;
pub mod request;
pub mod settings;
pub mod status;

pub use credentials::CredentialBundle;
pub use fleet::{CrewSpec, FleetSpec, GitAuth, GitIdentity, GitSettings};
pub use hook::{HOOK_EVENTS, HookEvent};
pub use plugin::{
    Capability, HelloRequest, HelloResponse, HookSubscriptions, PLUGIN_KIND, PLUGIN_PROTOCOL,
    PluginEntry, PluginManifest, PluginStatus, PluginsFile, SyncReport,
};
pub use protocol::{
    ActivateRequest, CHAIN_BUDGET_MS, DeactivateRequest, EventBatch, InterceptRequest,
    InterceptResponse, KvKeys, OBSERVER_BATCH, OBSERVER_QUEUE, PluginAction,
};
pub use record::{Desired, FleetRecord, Keep};
pub use request::{DownQuery, ErrorBody, FleetRequest, SessionRequest, SessionResponse};
pub use settings::{AgentSettings, ClaudeSettings, RunnerSettings};
pub use status::{
    ActivationState, AgentPhase, AgentStatus, FleetPhase, FleetStatus, FleetSummary,
    PluginActivation, SpecHash, Timestamp,
};

#[cfg(test)]
mod tests {
    use super::API_VERSION;

    #[test]
    fn api_version_is_v1() {
        assert_eq!(API_VERSION, "hecaton/v1");
    }
}
