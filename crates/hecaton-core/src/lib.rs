//! Domain model and ports (spec §3). No I/O lives here.

pub mod agent;
pub mod fakes;
pub mod fleet;
pub mod name;
pub mod ports;
pub mod repo;

pub use agent::{CrewRef, ResolvedAgent};
pub use fleet::{Crew, Fleet, FleetError};
pub use name::{AgentId, AgentName, CrewName, FleetName, NameError};
pub use ports::{
    AgentRunner, Clock, HookTarget, Keep, LaunchPlan, MaterializeError, Materializer,
    ObservedState, ProcessState, RunnerError, first_line,
};
pub use repo::{RepoError, RepoRef};
