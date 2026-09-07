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

/// A tiny base64 decoder: the fixtures' `raw` bodies are short ASCII.
fn b64(s: &str) -> Vec<u8> {
    let table: Vec<u8> = (b'A'..=b'Z')
        .chain(b'a'..=b'z')
        .chain(b'0'..=b'9')
        .chain(*b"+/")
        .collect();
    let mut bits = 0u32;
    let mut n = 0;
    let mut out = Vec::new();
    for c in s.bytes().filter(|c| *c != b'=') {
        bits = (bits << 6) | table.iter().position(|t| *t == c).unwrap() as u32;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
            bits &= (1 << n) - 1;
        }
    }
    out
}

fn fixture(name: &str) -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/plugin-protocol")
        .join(format!("{name}.json"));
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

#[tokio::test]
async fn the_client_sends_the_documented_bodies() {
    let metrics_body = String::from_utf8(b64(fixture("metrics")["raw"].as_str().unwrap())).unwrap();
    let stub = stub_plugin(StubScript {
        metrics_body: metrics_body.clone(),
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

    // `health.json` and `metrics.json` carry a `raw` body, not a request:
    // what the client makes of them is the assertion.
    let f = fixture("health");
    assert_eq!(f["request"], Value::Null);
    c.health(&stub.listen).await.unwrap();
    assert_eq!(stub.calls_named("health").len(), 1);

    let text = c
        .metrics(&stub.listen, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(text, metrics_body);
    assert!(
        text.starts_with("# HELP hecaton_plugin_flow_state"),
        "{text}"
    );
}
