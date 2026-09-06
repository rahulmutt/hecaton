//! Domain model and ports (spec §3). No I/O lives here.

pub mod agent;
pub mod events;
pub mod fakes;
pub mod fleet;
pub mod name;
pub mod ports;
pub mod reconcile;
pub mod repo;
pub mod store;

pub use agent::{CrewRef, ResolvedAgent};
pub use events::{EventHandler, Outcome, PassThrough};
pub use fleet::{Crew, Fleet, FleetError, RESERVED_AGENT_NAME};
pub use name::{AgentId, AgentName, CrewName, FleetName, NameError};
pub use ports::{
    AgentRunner, Clock, HookTarget, Keep, LaunchPlan, MaterializeError, Materializer,
    ObservedState, ProcessState, RunnerError, first_line,
};
pub use reconcile::{Plan, ReconcilePolicy, Step};
pub use repo::{RepoError, RepoRef};
pub use store::{Desired, FleetRecord, FleetSecrets, FleetStore, StoreError};
