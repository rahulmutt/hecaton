#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use assert_cmd::Command;
use hecaton_api::AgentPhase;
use hecaton_core::FleetRecord;
use hecaton_core::{AgentId, PassThrough};
use hecaton_server::testing::Harness;
use hecaton_server::{Daemon, Metrics, Ports, router, serve};
use predicates::prelude::*;

const PAYMENTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/payments.yaml");

/// A daemon on port 0 over the fakes; the binary finds it through the
/// endpoint and token files under this HOME.
struct Stub {
    home: tempfile::TempDir,
    url: String,
    daemon: Arc<Daemon>,
    _stop: tokio::sync::oneshot::Sender<()>,
    _rt: tokio::runtime::Runtime,
}

fn stub() -> Stub {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let h = Harness::new(Duration::from_secs(3600));
    let ports = Ports {
        materializer: h.materializer.clone(),
        runner: h.runner.clone(),
        clock: h.clock.clone(),
        store: h.store.clone(),
        policy: Default::default(),
        hook_url: "http://127.0.0.1:1".into(),
        resync: Duration::from_secs(3600),
    };
    let (daemon, url, stop) = rt.block_on(async {
        let daemon = Daemon::start(
            ports,
            Arc::new(PassThrough),
            Metrics::new().unwrap(),
            "tok".into(),
            Vec::new(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(serve(listener, router(daemon.clone()), async {
            let _ = rx.await;
        }));
        (daemon, url, tx)
    });
    let home = tempfile::tempdir().unwrap();
    let server = home.path().join(".local/state/hecaton/server");
    fs::create_dir_all(&server).unwrap();
    fs::write(server.join("endpoint"), &url).unwrap();
    fs::write(server.join("token"), "tok").unwrap();
    Stub {
        home,
        url,
        daemon,
        _stop: stop,
        _rt: rt,
    }
}

fn hecaton(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hecaton"));
    cmd.env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("HECATON_API_URL")
        .env_remove("CLAUDE_CONFIG_DIR");
    cmd
}

#[test]
fn up_status_list_update_and_down_through_the_binary() {
    let s = stub();
    let home = s.home.path();

    hecaton(home)
        .args(["up", PAYMENTS, "--no-host-defaults", "--no-wait"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "payments  pending  generation 1 (observed 0)",
        ));
    hecaton(home)
        .args(["up", PAYMENTS, "--no-host-defaults", "--no-wait"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("fleet exists"));

    // the fakes start every window but nothing sends SessionStart: up times out
    hecaton(home)
        .args(["update", PAYMENTS, "--no-host-defaults", "--timeout", "2s"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "timed out after 2s waiting for ready",
        ))
        .stderr(predicate::str::contains("payments/backend/alice  starting"));

    let out = hecaton(home)
        .args(["status", "payments", "--json"])
        .assert()
        .success();
    let rec: FleetRecord = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(rec.generation, 2);
    assert_eq!(
        rec.status.agents["payments/backend/bob"].phase,
        AgentPhase::Starting
    );

    hecaton(home)
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "NAME      PHASE        GEN  OBSERVED  AGENTS",
        ))
        .stdout(predicate::str::contains(
            "payments  reconciling  2    2         2",
        ));

    // ready both agents by hand, then `up`'s wait loop sees Ready
    for a in ["payments/backend/alice", "payments/backend/bob"] {
        let id: AgentId = a.parse().unwrap();
        let secret = s._rt.block_on(s.daemon.hook_secret(&id)).unwrap();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let resp = agent
            .post(&format!("{}/v1/agents/{a}/events", s.url))
            .header("Authorization", &format!("Bearer {secret}"))
            .send_json(serde_json::json!({ "hook_event_name": "SessionStart" }))
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }
    hecaton(home)
        .args(["update", PAYMENTS, "--no-host-defaults", "--timeout", "30s"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "payments  ready  generation 3 (observed 3)",
        ))
        .stderr(predicate::str::contains("payments/backend/alice: ready"));

    hecaton(home)
        .args(["down", "payments", "--purge", "--keep"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--purge cannot be combined"));
    hecaton(home)
        .args(["down", "payments", "--keep", "--timeout", "30s"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "payments  down  generation 3 (observed 3)",
        ));
    hecaton(home)
        .args(["down", "payments", "--purge", "--timeout", "30s"])
        .assert()
        .success()
        .stdout(predicate::str::contains("payments: purged"));
    hecaton(home)
        .args(["status", "payments"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("fleet payments not found"));
    hecaton(home)
        .args(["list"])
        .assert()
        .success()
        .stdout("no fleets\n");
}

#[test]
fn without_a_daemon_the_client_says_how_to_start_one() {
    let home = tempfile::tempdir().unwrap();
    hecaton(home.path())
        .args(["list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "daemon not running; run `hecaton serve -d`",
        ));
    hecaton(home.path())
        .args(["up", PAYMENTS, "--no-host-defaults", "--timeout", "zz"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid duration"));
}
