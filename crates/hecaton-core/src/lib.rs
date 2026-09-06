//! Domain model and ports (spec §3). No I/O lives here.

pub mod agent;
pub mod fleet;
pub mod name;
pub mod repo;

pub use agent::{CrewRef, ResolvedAgent};
pub use fleet::{Crew, Fleet, FleetError};
pub use name::{AgentId, AgentName, CrewName, FleetName, NameError};
pub use repo::{RepoError, RepoRef};
