//! The web plugin through the SDK harness (plugins spec §18.5): activation
//! validates, the index follows `fleets/watch`, the bridge echoes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::time::Duration;

use hecaton_api::{AgentPhase, FleetRecord, FleetSpec};
use hecaton_plugin_sdk::testing::{FakeHost, Harness};
use hecaton_plugin_sdk::{Env, Host};
use hecaton_plugin_web::WebPlugin;
use serde_json::{Value, json};

const ALICE: &str = "e2e/c/alice";
const BOB: &str = "e2e/c/bob";

fn fleet(agents: &[(&str, AgentPhase)]) -> FleetRecord {
    let mut r = FleetRecord::new(FleetSpec {
        name: "e2e".into(),
        crews: BTreeMap::new(),
    });
    for (id, phase) in agents {
        r.status.entry(id).phase = *phase;
    }
    r
}

async fn world() -> (FakeHost, Env, Harness, tokio::task::JoinHandle<()>) {
    let fake = FakeHost::start("tok", json!({}), vec![]).await;
    let env = fake.env("web", std::path::Path::new("/s"));
    let plugin = WebPlugin::new(Host::new(env.clone()).unwrap()).unwrap();
    let watch = plugin.start_watch();
    let h = Harness::start(&env, plugin).await;
    (fake, env, h, watch)
}

async fn rows(h: &Harness) -> Vec<Value> {
    let (status, _, body) = h.get_route("/agents.json", "/v1/plugins/web").await;
    assert_eq!(status, 200);
    serde_json::from_slice(&body).unwrap()
}

async fn wait_rows(h: &Harness, pred: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let r = rows(h).await;
            if pred(&r) {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the rows to settle")
}

#[tokio::test]
async fn activation_validates_and_the_index_follows_the_watch() {
    let (fake, _, h, _watch) = world().await;
    assert_eq!(h.activate(ALICE, json!({})).await, Ok(()));
    assert_eq!(h.activate(BOB, json!({ "enabled": false })).await, Ok(()));
    assert_eq!(
        h.activate("e2e/c/carol", json!({ "nope": 1 })).await,
        Err("nope: unknown field `nope`".into())
    );
    let r = wait_rows(&h, |r| r.len() == 1).await;
    assert_eq!(
        r[0],
        json!({ "id": ALICE, "phase": "pending", "message": "" }),
        "no frame yet"
    );

    fake.set_fleets(vec![fleet(&[
        (ALICE, AgentPhase::Ready),
        (BOB, AgentPhase::Ready),
    ])]);
    let r = wait_rows(&h, |r| r[0]["phase"] == "ready").await;
    assert_eq!(r.len(), 1, "bob is disabled");

    // enabling bob in place, and a config change back
    assert_eq!(h.activate(BOB, json!({ "enabled": true })).await, Ok(()));
    wait_rows(&h, |r| r.len() == 2 && r[1]["id"] == BOB).await;
    h.deactivate(ALICE).await;
    let r = wait_rows(&h, |r| r.len() == 1).await;
    assert_eq!(r[0]["id"], BOB);
    let _ = h.token();
}
