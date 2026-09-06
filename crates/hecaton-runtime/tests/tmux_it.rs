#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use hecaton_core::{AgentId, AgentRunner, LaunchPlan, ProcessState};
use hecaton_runtime::TmuxRunner;

fn wait_for(mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn session_window_observe_exit_respawn_and_teardown() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return;
    };
    let root = support::temp_root("tmux");
    let socket = format!("hecaton-test-{}", std::process::id());
    let r = TmuxRunner::new(tools.tmux.clone(), socket.clone());
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew = id.crew_ref();
    let fleet = id.fleet.clone();

    let agent_dir = root.join("a");
    std::fs::create_dir_all(agent_dir.join("logs")).unwrap();
    let script = agent_dir.join("launch.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho hello-from-agent\nexec sleep 300\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = LaunchPlan {
        cwd: agent_dir.clone(),
        env: BTreeMap::new(),
        argv: vec![],
        script: script.clone(),
    };

    assert!(
        r.observe(&fleet).unwrap().crews.is_empty(),
        "no server yet is not an error"
    );
    r.ensure_crew(&crew).unwrap();
    r.ensure_crew(&crew).unwrap();
    assert!(
        r.observe(&fleet).unwrap().crews[&crew.crew].is_empty(),
        "anchor window is not an agent"
    );

    r.ensure_agent(&id, &plan).unwrap();
    let pid = match r.observe(&fleet).unwrap().get(&id) {
        Some(ProcessState::Running { pid }) => *pid,
        other => panic!("expected running, got {other:?}"),
    };
    wait_for(|| {
        std::fs::read_to_string(agent_dir.join("logs").join("tmux.log"))
            .is_ok_and(|s| s.contains("hello-from-agent"))
    });

    // kill the script's process group leader → pane dead, remain-on-exit keeps the window
    assert!(
        std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    wait_for(|| {
        matches!(
            r.observe(&fleet).unwrap().get(&id),
            Some(ProcessState::Exited { .. })
        )
    });

    r.ensure_agent(&id, &plan).unwrap(); // respawn
    wait_for(
        || matches!(r.observe(&fleet).unwrap().get(&id), Some(ProcessState::Running { pid: p }) if *p != pid),
    );

    r.send_text(&id, "ignored", true).unwrap();
    r.stop_agent(&id).unwrap();
    r.stop_agent(&id).unwrap(); // absent → ok
    assert_eq!(r.observe(&fleet).unwrap().get(&id), None);
    assert!(
        r.observe(&fleet).unwrap().crews.contains_key(&crew.crew),
        "session survives via the anchor"
    );
    r.stop_crew(&crew).unwrap();
    r.stop_crew(&crew).unwrap();
    assert!(r.observe(&fleet).unwrap().crews.is_empty());
    let _ = std::process::Command::new(&tools.tmux)
        .args(["-L", &socket, "kill-server"])
        .status();
}
