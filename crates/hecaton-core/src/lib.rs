//! Domain model and ports (spec §3). No I/O lives here.

pub mod name;

pub use name::{AgentId, AgentName, CrewName, FleetName, NameError};
