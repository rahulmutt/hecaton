//! The daemon (Phase 3 spec §3): a registry of per-fleet actors over the
//! Phase 2 reconciler, a file store with an encrypted secrets vault, the
//! HTTP API, hook ingress and metrics. Depends on `hecaton-core` and
//! `hecaton-api` only; the binary wires the runtime adapters in.

pub mod actor;
pub mod auth;
pub mod fsutil;
pub mod hooks;
pub mod metrics;
pub mod store;
pub mod testing;
pub mod vault;

pub use actor::{FleetHandle, Msg, Ports, READY_EVENT, SecretIndex, Shared};
pub use auth::{RateLimiter, bearer, constant_time_eq};
pub use hooks::{ParsedEvent, parse_event};
pub use metrics::Metrics;
pub use store::FileFleetStore;
pub use vault::{Vault, VaultError, random_hex};
