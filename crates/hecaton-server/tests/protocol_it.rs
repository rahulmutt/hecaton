//! The daemon's client against the daemon-to-plugin fixtures: what
//! `PluginClient` puts on the wire is byte-for-byte the documented body.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use hecaton_api::{ActivateRequest, EventBatch, InterceptRequest};
use hecaton_server::PluginClient;
use hecaton_server::testing::{StubScript, stub_plugin};
use serde_json::{Value, json};

fn fixture(name: &str) -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/plugin-protocol")
        .join(format!("{name}.json"));
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

#[tokio::test]
async fn the_client_sends_the_documented_bodies() {
    let stub = stub_plugin(StubScript {
        reject: BTreeMap::from([(
            "payments/backend/bad".to_string(),
            "initial: unknown state \"nope\"".to_string(),
        )]),
        verdict: json!({ "decision": "block", "reason": "no recursive deletes" }),
        actions: vec![hecaton_api::PluginAction::SendText {
            text: "Use trash instead.".into(),
            submit: true,
        }],
        health_ok: true,
        ..StubScript::default()
    })
    .await;
    let c = PluginClient::new().unwrap();

    let f = fixture("activate");
    let req: ActivateRequest = serde_json::from_value(f["request"].clone()).unwrap();
    c.activate(&stub.listen, &req).await.unwrap();
    assert_eq!(stub.calls_named("activate")[0], f["request"]);

    let f = fixture("activate-rejected");
    let req: ActivateRequest = serde_json::from_value(f["request"].clone()).unwrap();
    let e = c.activate(&stub.listen, &req).await.unwrap_err();
    assert_eq!(
        e.to_string(),
        format!("HTTP 400: {}", f["response"]["error"].as_str().unwrap())
    );

    let f = fixture("events");
    let batch: EventBatch = serde_json::from_value(f["request"].clone()).unwrap();
    c.events(&stub.listen, &batch).await.unwrap();
    assert_eq!(stub.calls_named("events")[0], f["request"]);

    let f = fixture("intercept");
    let req: InterceptRequest = serde_json::from_value(f["request"].clone()).unwrap();
    let v = c
        .intercept(&stub.listen, &req, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(stub.calls_named("intercept")[0], f["request"]);
    assert_eq!(serde_json::to_value(&v).unwrap(), f["response"]);
}
