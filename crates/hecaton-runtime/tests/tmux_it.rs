#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use hecaton_core::{AgentId, AgentRunner, LaunchPlan, ProcessState};
use hecaton_runtime::TmuxRunner;
use hecaton_runtime::tmux::ATTACH_SESSION_PREFIX;

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
    let log = agent_dir.join("logs").join("tmux.log");
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
    wait_for(|| std::fs::read_to_string(&log).is_ok_and(|s| s.contains("hello-from-agent")));

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

    // Truncate first so this assertion can only pass if the respawn path
    // itself (not the leftover content from the first start) re-attaches
    // the pipe before the respawned script writes.
    std::fs::write(&log, "").unwrap();
    r.ensure_agent(&id, &plan).unwrap(); // respawn
    wait_for(
        || matches!(r.observe(&fleet).unwrap().get(&id), Some(ProcessState::Running { pid: p }) if *p != pid),
    );
    wait_for(|| std::fs::read_to_string(&log).is_ok_and(|s| s.contains("hello-from-agent")));

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

/// Plugins spec §18.4, §11.1: a real tmux attach through the PTY sees the
/// pane, typed bytes reach it, the resize reaches the client, the grouped
/// session dies with the stream, and `stop_crew` takes the whole group.
#[test]
fn attach_streams_the_pane_and_the_grouped_session_dies_with_the_stream() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return;
    };
    let root = support::temp_root("tmux-attach");
    let socket = format!("hecaton-test-attach-{}", std::process::id());
    let r = TmuxRunner::new(tools.tmux.clone(), socket.clone());
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew = id.crew_ref();
    let fleet = id.fleet.clone();
    let agent_dir = root.join("a");
    std::fs::create_dir_all(agent_dir.join("logs")).unwrap();
    let script = agent_dir.join("launch.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho hello-from-agent\nexec cat > typed.log\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = LaunchPlan {
        cwd: agent_dir.clone(),
        env: BTreeMap::new(),
        argv: vec![],
        script: script.clone(),
    };
    let tmux = |args: &[&str]| -> String {
        let out = std::process::Command::new(&tools.tmux)
            .args(["-L", &socket])
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let sessions = || tmux(&["list-sessions", "-F", "#{session_name}"]);

    assert!(r.attach(&id).is_err(), "no window yet");
    r.ensure_crew(&crew).unwrap();
    r.ensure_agent(&id, &plan).unwrap();
    wait_for(|| {
        std::fs::read_to_string(agent_dir.join("logs/tmux.log"))
            .is_ok_and(|s| s.contains("hello-from-agent"))
    });

    let stream = r.attach(&id).unwrap();
    let mut reader = stream.reader().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut seen = Vec::new();
    wait_for(|| {
        while let Ok(chunk) = rx.try_recv() {
            seen.extend(chunk);
        }
        String::from_utf8_lossy(&seen).contains("hello-from-agent")
    });
    let mut writer = stream.writer().unwrap();
    assert!(stream.writer().is_err(), "the writer is taken once");
    // Enter is CR, as a real terminal (and xterm.js) sends it: tmux drops a
    // bare LF from a client instead of passing it to the pane.
    writer.write_all(b"typed-through-attach\r").unwrap();
    writer.flush().unwrap();
    wait_for(|| {
        std::fs::read_to_string(agent_dir.join("typed.log"))
            .is_ok_and(|s| s.contains("typed-through-attach"))
    });
    stream.resize(100, 30).unwrap();
    wait_for(|| {
        tmux(&["list-clients", "-F", "#{client_width}x#{client_height}"]).contains("100x30")
    });
    let attach_session = sessions()
        .lines()
        .find(|l| l.starts_with(ATTACH_SESSION_PREFIX))
        .unwrap()
        .to_string();
    assert_eq!(
        tmux(&[
            "display-message",
            "-p",
            "-t",
            &attach_session,
            "#{window_name}"
        ])
        .trim(),
        "a",
        "the grouped session shows the agent's window"
    );
    assert_eq!(
        tmux(&["display-message", "-p", "-t", "f/c", "#{window_name}"]).trim(),
        "a",
        "the crew session's current window is whatever it was"
    );

    drop(stream);
    wait_for(|| !sessions().contains(ATTACH_SESSION_PREFIX));
    assert!(
        sessions().contains("f/c"),
        "the crew session survives the stream"
    );
    assert!(matches!(
        r.observe(&fleet).unwrap().get(&id),
        Some(ProcessState::Running { .. })
    ));

    // a second attach, then stop_crew must take the group with it
    let again = r.attach(&id).unwrap();
    wait_for(|| sessions().contains(ATTACH_SESSION_PREFIX));
    r.stop_crew(&crew).unwrap();
    wait_for(|| sessions().trim().is_empty());
    drop(again);
    let _ = std::process::Command::new(&tools.tmux)
        .args(["-L", &socket, "kill-server"])
        .status();
}
