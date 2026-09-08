//! The web plugin through the SDK harness (plugins spec §18.5): activation
//! validates, the index follows `fleets/watch`, the bridge echoes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use hecaton_api::{AgentPhase, FleetRecord, FleetSpec};
use hecaton_plugin_sdk::testing::{FakeHost, Harness, metric};
use hecaton_plugin_sdk::{Env, Host};
use hecaton_plugin_web::WebPlugin;
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

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

#[tokio::test]
async fn pages_link_through_the_forwarded_prefix_and_assets_are_served() {
    let (fake, _, h, _watch) = world().await;
    h.activate(ALICE, json!({})).await.unwrap();
    h.activate(BOB, json!({ "enabled": false })).await.unwrap();
    fake.set_fleets(vec![fleet(&[(ALICE, AgentPhase::Ready)])]);
    wait_rows(&h, |r| r[0]["phase"] == "ready").await;

    let (status, headers, body) = h.get_route("", "/v1/plugins/web").await;
    let body = String::from_utf8(body).unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "content-type" && v.starts_with("text/html"))
    );
    assert!(
        body.contains(r#"href="/v1/plugins/web/agents/e2e/c/alice""#),
        "{body}"
    );
    assert!(
        body.contains(r#"fetch("/v1/plugins/web/agents.json")"#),
        "{body}"
    );
    assert!(
        !body.contains("bob"),
        "disabled agents are not listed: {body}"
    );

    let (status, _, body) = h.get_route("/agents/e2e/c/alice", "/v1/plugins/web").await;
    let body = String::from_utf8(body).unwrap();
    assert_eq!(status, 200);
    assert!(
        body.contains(r#"src="/v1/plugins/web/assets/xterm.js""#),
        "{body}"
    );
    assert!(
        body.contains(r#"src="/v1/plugins/web/assets/addon-fit.js""#),
        "{body}"
    );
    assert!(
        body.contains(r#"href="/v1/plugins/web/assets/xterm.css""#),
        "{body}"
    );
    assert!(
        body.contains("/v1/plugins/web/agents/e2e/c/alice/ws"),
        "{body}"
    );
    assert!(body.contains("e2e/c/alice"), "{body}");
    let (status, _, _) = h.get_route("/agents/e2e/c/bob", "/v1/plugins/web").await;
    assert_eq!(status, 404, "hidden agents have no page");
    let (status, _, _) = h.get_route("/agents/e2e/c/bob/ws", "/v1/plugins/web").await;
    assert_ne!(status, 200, "and no bridge");

    for (file, kind, len) in [
        ("xterm.js", "text/javascript", 488663),
        ("xterm.css", "text/css", 7112),
        ("addon-fit.js", "text/javascript", 1521),
    ] {
        let (status, headers, body) = h
            .get_route(&format!("/assets/{file}"), "/v1/plugins/web")
            .await;
        assert_eq!(status, 200, "{file}");
        assert_eq!(body.len(), len, "{file}");
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "content-type" && v.starts_with(kind)),
            "{file}: {headers:?}"
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "cache-control" && v.contains("immutable")),
            "{file}"
        );
    }
    let (status, _, _) = h.get_route("/assets/nope.js", "/v1/plugins/web").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn the_bridge_relays_bytes_and_resizes_to_the_daemon_attach() {
    let (fake, env, h, _watch) = world().await;
    h.activate(ALICE, json!({})).await.unwrap();
    let url = format!("ws://{}/v1/routes/agents/e2e/c/alice/ws", h.listen());
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {}", env.token).parse().unwrap(),
    );
    let (mut ws, _) = connect_async(req).await.unwrap();
    ws.send(Message::Binary(b"ls\n".to_vec().into()))
        .await
        .unwrap();
    let echo = ws.next().await.unwrap().unwrap();
    assert_eq!(
        echo.into_data().as_ref(),
        b"ls\n",
        "browser → plugin → daemon(fake) → back"
    );
    ws.send(Message::Text(r#"{"resize":{"cols":100,"rows":30}}"#.into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while fake.resizes().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        fake.resizes()[0],
        (
            ALICE.to_string(),
            json!({ "resize": { "cols": 100, "rows": 30 } })
        )
    );
    assert_eq!(fake.attaches(), vec![ALICE.to_string()]);
    let text = h.metrics().await;
    assert_eq!(
        metric(&text, "hecaton_plugin_web_terminals_open", &[]),
        Some(1.0)
    );
    assert_eq!(
        metric(&text, "hecaton_plugin_web_terminals_total", &[]),
        Some(1.0)
    );
    ws.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while metric(&h.metrics().await, "hecaton_plugin_web_terminals_open", &[]) != Some(0.0) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the gauge falls when the browser leaves");
    // a bridge for a hidden agent is refused before any attach
    let mut req = format!("ws://{}/v1/routes/agents/e2e/c/bob/ws", h.listen())
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {}", env.token).parse().unwrap(),
    );
    assert!(connect_async(req).await.is_err());
    assert_eq!(fake.attaches().len(), 1);
}
