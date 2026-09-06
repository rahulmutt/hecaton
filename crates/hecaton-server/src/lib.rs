//! The daemon (Phase 3 spec §3): a registry of per-fleet actors over the
//! Phase 2 reconciler, a file store with an encrypted secrets vault, the
//! HTTP API, hook ingress and metrics. Depends on `hecaton-core` and
//! `hecaton-api` only; the binary wires the runtime adapters in.

pub mod fsutil;
pub mod store;
pub mod vault;

pub use store::FileFleetStore;
pub use vault::{Vault, VaultError, random_hex};
