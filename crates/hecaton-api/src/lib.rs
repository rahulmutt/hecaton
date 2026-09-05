//! Wire types shared by the hecaton CLI and daemon (spec §3).
//!
//! This crate is a leaf: serde DTOs only, no logic beyond defaults and
//! secret-redacting `Debug` impls.

/// The `apiVersion` every fleet file and request declares.
pub const API_VERSION: &str = "hecaton/v1";

pub mod credentials;
pub mod fleet;
pub mod request;
pub mod settings;

pub use credentials::CredentialBundle;
pub use fleet::{CrewSpec, FleetSpec, GitAuth, GitSettings};
pub use request::FleetRequest;
pub use settings::{AgentSettings, ClaudeSettings, RunnerSettings};

#[cfg(test)]
mod tests {
    use super::API_VERSION;

    #[test]
    fn api_version_is_v1() {
        assert_eq!(API_VERSION, "hecaton/v1");
    }
}
