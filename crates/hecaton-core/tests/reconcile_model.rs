//! Model-based test (Phase 2 spec §3, §6): the reconciler against a reference
//! model over random up / update / down / exit / ready / tick sequences.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use hecaton_api::{
    AgentPhase, AgentSettings, CredentialBundle, CrewSpec, FleetPhase, FleetSpec, FleetStatus,
    GitSettings, Timestamp,
};
use hecaton_core::fakes::{FakeClock, FakeMaterializer, FakeRunner};
use hecaton_core::reconcile::{ReconcileContext, ReconcilePolicy, Step, reconcile_pass};
use hecaton_core::{AgentId, Clock, Fleet, FleetName, HookTarget, Keep, ProcessState};
use proptest::prelude::*;
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest, prop_state_machine};

const AGENTS: [&str; 3] = ["a", "b", "c"];
const MAX_RESTARTS: u32 = 2;
const BASE: u64 = 2;
const CAP: u64 = 3;

fn policy() -> ReconcilePolicy {
    ReconcilePolicy {
        max_restarts: MAX_RESTARTS,
        backoff_base_secs: BASE,
        backoff_cap_secs: CAP,
    }
}

fn backoff(restarts: u32) -> u64 {
    (BASE << (restarts - 1)).min(CAP)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RefAgent {
    phase: AgentPhase,
    restarts: u32,
    next_restart_at: Option<u64>,
    version: u32,
}

#[derive(Clone, Debug, Default)]
struct RefState {
    desired: Option<BTreeMap<String, u32>>,
    agents: BTreeMap<String, RefAgent>,
    now: u64,
}

#[derive(Clone, Debug)]
enum Transition {
    Up(BTreeMap<String, u32>),
    Update(String, u32),
    Down,
    AgentExits(String),
    AgentReady(String),
    Tick(u64),
}

struct Model;

impl ReferenceStateMachine for Model {
    type State = RefState;
    type Transition = Transition;

    fn init_state() -> BoxedStrategy<Self::State> {
        Just(RefState {
            now: 1_000,
            ..RefState::default()
        })
        .boxed()
    }

    fn transitions(state: &Self::State) -> BoxedStrategy<Self::Transition> {
        let up = proptest::collection::btree_map(
            proptest::sample::select(AGENTS.to_vec()).prop_map(String::from),
            0..3u32,
            1..=3,
        )
        .prop_map(Transition::Up);
        let tick = (1..12u64).prop_map(Transition::Tick);
        let names: Vec<String> = state.agents.keys().cloned().collect();
        if names.is_empty() {
            return prop_oneof![up, Just(Transition::Down), tick].boxed();
        }
        let pick = proptest::sample::select(names);
        prop_oneof![
            2 => up,
            2 => (pick.clone(), 0..3u32).prop_map(|(a, v)| Transition::Update(a, v)),
            1 => Just(Transition::Down),
            3 => pick.clone().prop_map(Transition::AgentExits),
            3 => pick.prop_map(Transition::AgentReady),
            3 => tick,
        ]
        .boxed()
    }

    fn preconditions(state: &Self::State, t: &Self::Transition) -> bool {
        match t {
            Transition::Update(a, _) => state.desired.as_ref().is_some_and(|d| d.contains_key(a)),
            // only while the window is actually running: a pending restart
            // (next_restart_at set) means the window is already Exited
            Transition::AgentExits(a) | Transition::AgentReady(a) => {
                state.agents.get(a).is_some_and(|x| {
                    matches!(x.phase, AgentPhase::Starting | AgentPhase::Ready)
                        && x.next_restart_at.is_none()
                })
            }
            _ => true,
        }
    }

    /// The state after the transition AND one reconcile pass.
    fn apply(mut s: Self::State, t: &Self::Transition) -> Self::State {
        match t {
            Transition::Up(map) => {
                s.desired = Some(map.clone());
                s.agents.retain(|k, _| map.contains_key(k));
                for (name, v) in map {
                    let changed = s.agents.get(name).is_none_or(|a| a.version != *v);
                    if changed {
                        s.agents.insert(
                            name.clone(),
                            RefAgent {
                                phase: AgentPhase::Starting,
                                restarts: 0,
                                next_restart_at: None,
                                version: *v,
                            },
                        );
                    }
                }
            }
            Transition::Update(name, v) => {
                if let Some(d) = s.desired.as_mut() {
                    d.insert(name.clone(), *v);
                }
                if s.agents.get(name).is_some_and(|a| a.version != *v) {
                    s.agents.insert(
                        name.clone(),
                        RefAgent {
                            phase: AgentPhase::Starting,
                            restarts: 0,
                            next_restart_at: None,
                            version: *v,
                        },
                    );
                }
            }
            Transition::Down => {
                s.desired = None;
                s.agents.clear();
            }
            Transition::AgentExits(name) => {
                if let Some(a) = s.agents.get_mut(name) {
                    a.restarts += 1;
                    if a.restarts > MAX_RESTARTS {
                        a.phase = AgentPhase::Dead;
                        a.next_restart_at = None;
                    } else {
                        a.next_restart_at = Some(s.now + backoff(a.restarts));
                    }
                }
            }
            Transition::AgentReady(name) => {
                if let Some(a) = s.agents.get_mut(name) {
                    a.phase = AgentPhase::Ready;
                    a.restarts = 0;
                    a.next_restart_at = None;
                }
            }
            Transition::Tick(secs) => {
                s.now += secs;
            }
        }
        // the pass after the transition restarts every due agent
        for a in s.agents.values_mut() {
            if a.next_restart_at.is_some_and(|t| t <= s.now) {
                a.phase = AgentPhase::Starting;
                a.next_restart_at = None;
            }
        }
        s
    }
}

struct Sut {
    status: FleetStatus,
    m: FakeMaterializer,
    r: FakeRunner,
    clock: FakeClock,
    desired: Option<Fleet>,
    fleet: FleetName,
    creds: CredentialBundle,
}

fn fleet_of(map: &BTreeMap<String, u32>) -> Fleet {
    Fleet::try_from(FleetSpec {
        name: "f".into(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/api".into(),
                git_ref: "main".into(),
                git: GitSettings::default(),
                agents: map
                    .iter()
                    .map(|(n, v)| {
                        let mut s = AgentSettings::default();
                        s.env.insert("V".into(), v.to_string());
                        (n.clone(), s)
                    })
                    .collect(),
            },
        )]),
    })
    .unwrap()
}

fn hooks(id: &AgentId) -> HookTarget {
    HookTarget {
        url: "https://127.0.0.1:7643".into(),
        secret: id.to_string(),
    }
}

fn id(name: &str) -> AgentId {
    format!("f/c/{name}").parse().unwrap()
}

impl Sut {
    fn pass(&mut self) -> Vec<Step> {
        let ctx = ReconcileContext {
            fleet: &self.fleet,
            desired: self.desired.as_ref(),
            keep: Keep::default(),
            materializer: &self.m,
            runner: &self.r,
            creds: &self.creds,
            hooks: &hooks,
            policy: &policy(),
            clock: &self.clock,
        };
        let (plan, report) = reconcile_pass(&mut self.status, &ctx).unwrap();
        assert!(
            report.all_ok(),
            "no failures are injected in this model: {report:?}"
        );
        plan
    }
}

impl StateMachineTest for Sut {
    type SystemUnderTest = Sut;
    type Reference = Model;

    fn init_test(_: &RefState) -> Self::SystemUnderTest {
        let mut sut = Sut {
            status: FleetStatus::default(),
            m: FakeMaterializer::default(),
            r: FakeRunner::default(),
            clock: FakeClock::new(Timestamp(1_000)),
            desired: None,
            fleet: "f".parse().unwrap(),
            creds: CredentialBundle::default(),
        };
        // `proptest-state-machine` checks invariants on the initial state too
        // (before any transition is applied), and the model's init state
        // (`desired: None`, no agents) is defined in terms of a fleet phase
        // that only exists after a reconcile pass has run (spec §3.4:
        // `Terminating` if desired is `None`, `Pending` for no agents). Run
        // one pass up front so the SUT's `FleetStatus::default()` reaches the
        // same already-settled state the reference model starts in.
        sut.pass();
        sut
    }

    fn apply(mut sut: Self::SystemUnderTest, _: &RefState, t: Transition) -> Self::SystemUnderTest {
        match t {
            Transition::Up(map) => {
                sut.desired = Some(fleet_of(&map));
                sut.status.generation += 1;
            }
            Transition::Update(name, v) => {
                if let Some(f) = sut.desired.as_mut() {
                    let mut s = f
                        .crews
                        .get_mut(&"c".parse().unwrap())
                        .unwrap()
                        .agents
                        .get(&name.parse().unwrap())
                        .cloned()
                        .unwrap();
                    s.env.insert("V".into(), v.to_string());
                    f.crews
                        .get_mut(&"c".parse().unwrap())
                        .unwrap()
                        .agents
                        .insert(name.parse().unwrap(), s);
                }
                sut.status.generation += 1;
            }
            Transition::Down => sut.desired = None,
            Transition::AgentExits(name) => sut
                .r
                .set_state(&id(&name), ProcessState::Exited { code: Some(1) }),
            Transition::AgentReady(name) => {
                hecaton_core::reconcile::agent_ready(&mut sut.status, &id(&name), sut.clock.now())
            }
            Transition::Tick(secs) => sut.clock.advance(secs),
        }
        sut.pass();
        sut
    }

    fn check_invariants(sut: &Self::SystemUnderTest, r: &RefState) {
        let got: BTreeMap<String, RefAgent> = sut
            .status
            .agents
            .iter()
            .map(|(k, a)| {
                let name = k.rsplit('/').next().unwrap().to_string();
                let version = r.agents.get(&name).map_or(u32::MAX, |x| x.version);
                (
                    name,
                    RefAgent {
                        phase: a.phase,
                        restarts: a.restarts,
                        next_restart_at: a.next_restart_at.map(|t| t.0),
                        version,
                    },
                )
            })
            .collect();
        assert_eq!(got, r.agents, "status vs model\nstatus: {:#?}", sut.status);

        let observed = sut.r.observed();
        for (name, a) in &r.agents {
            let running = matches!(observed.get(&id(name)), Some(ProcessState::Running { .. }));
            let should_run = matches!(a.phase, AgentPhase::Starting | AgentPhase::Ready)
                && a.next_restart_at.is_none();
            assert_eq!(running, should_run, "{name}: process state vs phase {a:?}");
        }

        let expected_fleet = if r.desired.is_none() {
            FleetPhase::Terminating
        } else if r.agents.values().any(|a| a.phase == AgentPhase::Dead) {
            FleetPhase::Degraded
        } else if r.agents.values().any(|a| a.phase == AgentPhase::Starting) {
            FleetPhase::Reconciling
        } else if r.agents.is_empty() {
            FleetPhase::Pending
        } else {
            FleetPhase::Ready
        };
        assert_eq!(sut.status.phase, expected_fleet);
        assert_eq!(sut.status.observed_generation, sut.status.generation);

        // idempotence: a second pass with the same inputs only re-ensures crews
        let mut again = Sut {
            status: sut.status.clone(),
            m: FakeMaterializer::default(),
            r: FakeRunner::default(),
            clock: FakeClock::new(sut.clock.now()),
            desired: sut.desired.clone(),
            fleet: sut.fleet.clone(),
            creds: CredentialBundle::default(),
        };
        for (crew, agents) in &observed.crews {
            for (agent, s) in agents {
                again.r.set_state(
                    &AgentId {
                        fleet: sut.fleet.clone(),
                        crew: crew.clone(),
                        agent: agent.clone(),
                    },
                    *s,
                );
            }
        }
        let plan = again.pass();
        assert!(
            plan.iter().all(|s| matches!(s, Step::EnsureCrew(_))),
            "second pass not idempotent: {plan:?}"
        );
    }
}

prop_state_machine! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]
    #[test]
    fn reconciler_matches_the_reference_model(sequential 1..40 => Sut);
}

/// Spec §6's model row also covers "runs where a named step is told to
/// fail". The reference state machine above never injects a failure (it
/// would need to model the Stop→fail intermediate phases the spec does not
/// define), so this is a separate, narrower property: arm exactly one
/// failing step on a fresh fleet, check the failed pass's generic
/// consequences (degraded, no generation bump, right messages, nothing
/// half-started), then check that a clean pass afterwards converges and a
/// third pass is idempotent.
#[derive(Clone, Debug)]
enum Failure {
    EnsureCrew,
    Materialize(String),
    EnsureAgent(String),
}

fn fleet_and_failure() -> impl Strategy<Value = (BTreeMap<String, u32>, Failure)> {
    proptest::collection::btree_map(
        proptest::sample::select(AGENTS.to_vec()).prop_map(String::from),
        0..3u32,
        1..=3,
    )
    .prop_flat_map(|map| {
        let names: Vec<String> = map.keys().cloned().collect();
        let failure = prop_oneof![
            Just(Failure::EnsureCrew),
            proptest::sample::select(names.clone()).prop_map(Failure::Materialize),
            proptest::sample::select(names).prop_map(Failure::EnsureAgent),
        ];
        (Just(map), failure)
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]
    #[test]
    fn a_failed_step_degrades_the_pass_and_the_next_clean_pass_converges(
        (map, failure) in fleet_and_failure()
    ) {
        let names: Vec<String> = map.keys().cloned().collect();
        let fleet = fleet_of(&map);
        let m = FakeMaterializer::default();
        let r = FakeRunner::default();
        let clock = FakeClock::new(Timestamp(1_000));
        let creds = CredentialBundle::default();
        let fleet_name: FleetName = "f".parse().unwrap();
        let mut status = FleetStatus { generation: 1, ..FleetStatus::default() };
        let desired = Some(fleet);

        match &failure {
            Failure::EnsureCrew => m.fail_next("ensure_crew", "f/c", "boom"),
            Failure::Materialize(name) => m.fail_next("materialize", &format!("f/c/{name}"), "boom"),
            Failure::EnsureAgent(name) => r.fail_next("ensure_agent", &format!("f/c/{name}"), "boom"),
        }

        let ctx = ReconcileContext {
            fleet: &fleet_name,
            desired: desired.as_ref(),
            keep: Keep::default(),
            materializer: &m,
            runner: &r,
            creds: &creds,
            hooks: &hooks,
            policy: &policy(),
            clock: &clock,
        };

        // Pass 1: the armed failure fires.
        let (_, report) = reconcile_pass(&mut status, &ctx).unwrap();
        prop_assert!(!report.all_ok());
        prop_assert_eq!(status.phase, FleetPhase::Degraded);
        prop_assert_eq!(status.observed_generation, 0);

        match &failure {
            Failure::EnsureCrew => {
                for name in &names {
                    let a = status.agents.get(&format!("f/c/{name}")).unwrap();
                    prop_assert!(!a.message.is_empty());
                }
                prop_assert!(!r.calls().iter().any(|c| c.starts_with("ensure_agent")));
            }
            Failure::Materialize(failing) => {
                let a = status.agents.get(&format!("f/c/{failing}")).unwrap();
                prop_assert!(!a.message.is_empty());
                prop_assert_ne!(a.phase, AgentPhase::Starting);
                let call = format!("ensure_agent f/c/{failing}");
                prop_assert!(!r.calls().contains(&call));
                for name in &names {
                    if name != failing {
                        let a = status.agents.get(&format!("f/c/{name}")).unwrap();
                        prop_assert_eq!(a.phase, AgentPhase::Starting);
                    }
                }
            }
            Failure::EnsureAgent(failing) => {
                let a = status.agents.get(&format!("f/c/{failing}")).unwrap();
                prop_assert!(!a.message.is_empty());
                prop_assert_ne!(a.phase, AgentPhase::Starting);
                let call = format!("ensure_agent f/c/{failing}");
                prop_assert!(r.calls().contains(&call));
                let running = matches!(
                    r.observed().get(&id(failing)),
                    Some(ProcessState::Running { .. })
                );
                prop_assert!(!running);
                for name in &names {
                    if name != failing {
                        let a = status.agents.get(&format!("f/c/{name}")).unwrap();
                        prop_assert_eq!(a.phase, AgentPhase::Starting);
                    }
                }
            }
        }

        // Pass 2: no failure armed this time; the fleet converges.
        let (_, report2) = reconcile_pass(&mut status, &ctx).unwrap();
        prop_assert!(report2.all_ok());
        prop_assert_eq!(status.observed_generation, status.generation);
        for name in &names {
            let a = status.agents.get(&format!("f/c/{name}")).unwrap();
            prop_assert_eq!(a.phase, AgentPhase::Starting);
        }
        prop_assert_eq!(status.phase, FleetPhase::Reconciling);

        // Pass 3: idempotent, only EnsureCrew.
        let (plan, _) = reconcile_pass(&mut status, &ctx).unwrap();
        prop_assert!(plan.iter().all(|s| matches!(s, Step::EnsureCrew(_))));
    }
}
