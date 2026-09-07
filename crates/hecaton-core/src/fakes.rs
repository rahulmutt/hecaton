//! In-memory ports for tests (Phase 2 spec §3.5). Always compiled: they are
//! small, dependency-free, and Phase 3's tests need them too.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use hecaton_api::{CredentialBundle, GitSettings, Timestamp};

use crate::agent::{CrewRef, ResolvedAgent};
use crate::name::{AgentId, AgentName, FleetName};
use crate::plugin::ResolvedPlugin;
use crate::ports::{
    AgentRunner, Clock, HookTarget, Keep, LaunchPlan, MaterializeError, Materializer,
    ObservedState, ProcessState, RunnerError,
};
use crate::repo::RepoRef;

#[derive(Default)]
struct Recorder {
    calls: Vec<String>,
    fail_next: Vec<(String, String, String)>, // (method, id, stderr)
}

impl Recorder {
    /// Records the call; returns the stderr to fail with, if one was armed.
    fn record(&mut self, method: &str, id: &str) -> Option<String> {
        self.calls.push(format!("{method} {id}"));
        let pos = self
            .fail_next
            .iter()
            .position(|(m, i, _)| m == method && i == id)?;
        Some(self.fail_next.remove(pos).2)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
pub struct FakeMaterializer {
    rec: Mutex<Recorder>,
}

impl FakeMaterializer {
    pub fn calls(&self) -> Vec<String> {
        lock(&self.rec).calls.clone()
    }
    pub fn fail_next(&self, method: &str, id: &str, stderr: &str) {
        lock(&self.rec)
            .fail_next
            .push((method.into(), id.into(), stderr.into()));
    }
    fn check(&self, method: &str, id: &str, tool: &str) -> Result<(), MaterializeError> {
        match lock(&self.rec).record(method, id) {
            Some(stderr) => Err(MaterializeError::Tool {
                id: id.into(),
                tool: tool.into(),
                subcommand: method.into(),
                args: vec![],
                stderr,
            }),
            None => Ok(()),
        }
    }
}

impl Materializer for FakeMaterializer {
    fn ensure_crew(
        &self,
        crew: &CrewRef,
        _: &RepoRef,
        _: &str,
        _: &GitSettings,
        _: &CredentialBundle,
    ) -> Result<(), MaterializeError> {
        self.check("ensure_crew", &crew.to_string(), "git")
    }
    fn materialize(
        &self,
        agent: &ResolvedAgent,
        _: &CredentialBundle,
        _: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        let id = agent.id.to_string();
        self.check("materialize", &id, "mise")?;
        let root = PathBuf::from("/fake").join(&id);
        Ok(LaunchPlan {
            cwd: root.join("workspace"),
            env: BTreeMap::new(),
            argv: vec!["fake".into()],
            script: root.join("launch.sh"),
        })
    }
    fn remove_agent(&self, agent: &AgentId) -> Result<(), MaterializeError> {
        self.check("remove_agent", &agent.to_string(), "rm")
    }
    fn remove_crew(&self, crew: &CrewRef, keep: Keep) -> Result<(), MaterializeError> {
        let tag = format!("{crew} repos={} sessions={}", keep.repos, keep.sessions);
        self.check("remove_crew", &tag, "rm").map_err(|e| match e {
            MaterializeError::Tool {
                tool,
                subcommand,
                args,
                stderr,
                ..
            } => MaterializeError::Tool {
                id: crew.to_string(),
                tool,
                subcommand,
                args,
                stderr,
            },
            other => other,
        })
    }
    fn materialize_plugin(
        &self,
        plugin: &ResolvedPlugin,
        _: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        let id = plugin.id().to_string();
        self.check("materialize_plugin", &id, "mise")?;
        Ok(LaunchPlan {
            cwd: plugin.package.clone(),
            env: BTreeMap::new(),
            argv: vec!["fake-plugin".into()],
            script: PathBuf::from("/fake").join(&id).join("launch.sh"),
        })
    }
    fn purge_plugin(&self, name: &AgentName) -> Result<(), MaterializeError> {
        self.check("purge_plugin", name.as_str(), "rm")
    }
}

#[derive(Default)]
pub struct FakeRunner {
    rec: Mutex<Recorder>,
    state: Mutex<ObservedState>,
    next_pid: Mutex<u32>,
    /// While set, every `observe()` call fails (spec §3.2: a failed
    /// `observe()` is logged, counted, leaves status unchanged, and the
    /// next tick retries at the resync cadence).
    fail_observe: AtomicBool,
}

impl FakeRunner {
    pub fn calls(&self) -> Vec<String> {
        lock(&self.rec).calls.clone()
    }
    pub fn fail_next(&self, method: &str, id: &str, stderr: &str) {
        lock(&self.rec)
            .fail_next
            .push((method.into(), id.into(), stderr.into()));
    }
    pub fn set_state(&self, id: &AgentId, s: ProcessState) {
        lock(&self.state).set(id, s);
    }
    pub fn observed(&self) -> ObservedState {
        lock(&self.state).clone()
    }
    /// Makes every subsequent `observe()` fail (or, passed `false`, stops
    /// failing) until changed again.
    pub fn set_fail_observe(&self, fail: bool) {
        self.fail_observe.store(fail, Ordering::SeqCst);
    }
    fn check(&self, method: &str, id: &str) -> Result<(), RunnerError> {
        match lock(&self.rec).record(method, id) {
            Some(stderr) => Err(RunnerError::Tool {
                id: id.into(),
                subcommand: method.into(),
                args: vec![],
                stderr,
            }),
            None => Ok(()),
        }
    }
}

impl AgentRunner for FakeRunner {
    fn ensure_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        self.check("ensure_crew", &crew.to_string())?;
        lock(&self.state)
            .crews
            .entry(crew.crew.clone())
            .or_default();
        Ok(())
    }
    fn ensure_agent(&self, agent: &AgentId, _: &LaunchPlan) -> Result<(), RunnerError> {
        self.check("ensure_agent", &agent.to_string())?;
        let pid = {
            let mut p = lock(&self.next_pid);
            *p += 1;
            *p
        };
        lock(&self.state).set(agent, ProcessState::Running { pid });
        Ok(())
    }
    fn stop_agent(&self, agent: &AgentId) -> Result<(), RunnerError> {
        self.check("stop_agent", &agent.to_string())?;
        lock(&self.state).remove(agent);
        Ok(())
    }
    fn stop_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        self.check("stop_crew", &crew.to_string())?;
        lock(&self.state).crews.remove(&crew.crew);
        Ok(())
    }
    fn observe(&self, fleet: &FleetName) -> Result<ObservedState, RunnerError> {
        self.check("observe", fleet.as_str())?;
        if self.fail_observe.load(Ordering::SeqCst) {
            return Err(RunnerError::Tool {
                id: fleet.as_str().into(),
                subcommand: "observe".into(),
                args: vec![],
                stderr: "fake observe failure".into(),
            });
        }
        Ok(self.observed())
    }
    fn send_text(&self, agent: &AgentId, text: &str, submit: bool) -> Result<(), RunnerError> {
        self.check("send_text", &format!("{agent} {text:?} submit={submit}"))
    }
}

pub struct FakeClock(Mutex<Timestamp>);

impl FakeClock {
    pub fn new(t: Timestamp) -> Self {
        Self(Mutex::new(t))
    }
    pub fn set(&self, t: Timestamp) {
        *lock(&self.0) = t;
    }
    pub fn advance(&self, secs: u64) {
        let mut t = lock(&self.0);
        *t = t.plus_secs(secs);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        *lock(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> AgentId {
        s.parse().unwrap()
    }

    #[test]
    fn runner_tracks_windows_and_records_calls() {
        let r = FakeRunner::default();
        let crew: CrewRef = "f/c".parse().unwrap();
        r.ensure_crew(&crew).unwrap();
        r.ensure_agent(
            &id("f/c/a"),
            &LaunchPlan {
                cwd: "/x".into(),
                env: BTreeMap::new(),
                argv: vec![],
                script: "/x/l".into(),
            },
        )
        .unwrap();
        assert_eq!(
            r.observe(&"f".parse().unwrap()).unwrap().get(&id("f/c/a")),
            Some(&ProcessState::Running { pid: 1 })
        );
        r.stop_agent(&id("f/c/a")).unwrap();
        assert_eq!(r.observed().get(&id("f/c/a")), None);
        assert_eq!(
            r.calls(),
            vec![
                "ensure_crew f/c",
                "ensure_agent f/c/a",
                "observe f",
                "stop_agent f/c/a"
            ]
        );
    }

    #[test]
    fn fail_next_fails_exactly_once_for_the_named_call() {
        let m = FakeMaterializer::default();
        m.fail_next("remove_agent", "f/c/a", "boom");
        let e = m.remove_agent(&id("f/c/a")).unwrap_err();
        assert_eq!(e.to_string(), "f/c/a: rm remove_agent: boom");
        assert!(m.remove_agent(&id("f/c/a")).is_ok());
        assert!(m.remove_agent(&id("f/c/b")).is_ok());
    }

    #[test]
    fn plugin_calls_are_recorded_and_failable() {
        let m = FakeMaterializer::default();
        let p = ResolvedPlugin {
            name: "web".parse().unwrap(),
            package: "/pkg/web".into(),
            manifest: serde_json::from_value(serde_json::json!({
                "apiVersion": "hecaton/v1", "kind": "Plugin", "name": "web",
                "version": "0.1.0", "protocol": 1, "start": "serve"
            }))
            .unwrap(),
            config: serde_json::json!({}),
            digest: None,
        };
        let host = HookTarget {
            url: "http://127.0.0.1:1".into(),
            secret: "tok".into(),
        };
        let plan = m.materialize_plugin(&p, &host).unwrap();
        assert_eq!(plan.cwd, PathBuf::from("/pkg/web"));
        m.fail_next("purge_plugin", "web", "busy");
        assert_eq!(
            m.purge_plugin(&"web".parse().unwrap())
                .unwrap_err()
                .to_string(),
            "web: rm purge_plugin: busy"
        );
        assert_eq!(
            m.calls(),
            vec!["materialize_plugin hecaton/plugins/web", "purge_plugin web"]
        );
    }

    #[test]
    fn clock_advances() {
        let c = FakeClock::new(Timestamp(100));
        c.advance(5);
        assert_eq!(c.now(), Timestamp(105));
        c.set(Timestamp(1));
        assert_eq!(c.now(), Timestamp(1));
    }
}
