//! Driven adapters (Phase 2 spec §4): everything that turns a `ResolvedAgent`
//! into files and a tmux window. Every path comes from `StateLayout`, every
//! binary from `ToolPaths`; nothing here reads the process environment.

pub mod fsutil;
pub mod layout;
pub mod quote;
pub mod tools;

pub use layout::{AgentPaths, CrewPaths, StateLayout};
pub use quote::sh_quote;
pub use tools::{MissingTool, ToolPaths};
