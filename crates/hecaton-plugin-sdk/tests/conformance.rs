//! The SDK against `docs/plugin-protocol/` (plugins spec §4, §10):
//! every daemon-to-plugin fixture through `router`, every plugin-to-daemon
//! fixture through `Host` against `FakeHost`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hecaton_api::{FleetRecord, HookEvent, InterceptResponse, PluginAction};
use hecaton_plugin_sdk::testing::FakeHost;
use hecaton_plugin_sdk::{Host, Plugin, bind, run};
use serde_json::{Value, json};

fn fixtures() -> BTreeMap<String, Value> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/plugin-protocol");
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let p: PathBuf = entry.unwrap().path();
        if p.extension().is_some_and(|e| e == "json") {
            let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
            out.insert(p.file_stem().unwrap().to_string_lossy().into_owned(), v);
        }
    }
    assert_eq!(
        out.len(),
        14,
        "every fixture accounted for: {:?}",
        out.keys()
    );
    out
}

fn b64(s: &str) -> Vec<u8> {
    // tiny base64 decoder: fixtures only carry short ASCII payloads
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

/// The reference plugin the daemon-to-plugin fixtures describe.
struct Reference;
impl Plugin for Reference {
    async fn activate(&self, _agent: &str, config: Value) -> Result<(), String> {
        match config["initial"].as_str() {
            Some("working") => Ok(()),
            Some(other) => Err(format!("initial: unknown state {other:?}")),
            None => Err("initial: missing".into()),
        }
    }
    async fn intercept(
        &self,
        event: HookEvent,
        _so_far: Value,
        _deadline: u64,
    ) -> InterceptResponse {
        assert_eq!(event.payload["tool_input"]["command"], "rm -rf /");
        InterceptResponse {
            response: json!({ "decision": "block", "reason": "no recursive deletes" }),
            actions: vec![PluginAction::SendText {
                text: "Use trash instead.".into(),
                submit: true,
            }],
        }
    }
    async fn metrics(&self) -> String {
        "# TYPE hecaton_plugin_flow_state gauge\nhecaton_plugin_flow_state{agent=\"bob\",state=\"working\"} 1\n".into()
    }
}

#[tokio::test]
async fn the_router_answers_every_daemon_to_plugin_fixture() {
    let (listener, listen) = bind().await.unwrap();
    tokio::spawn(run(listener, Arc::new(Reference)));
    let c = reqwest::Client::builder().no_proxy().build().unwrap();
    for (name, f) in fixtures()
        .iter()
        .filter(|(_, f)| f["direction"] == "daemon-to-plugin")
    {
        let (method, path) = f["route"].as_str().unwrap().split_once(' ').unwrap();
        let url = format!("http://{listen}{path}");
        let req = match method {
            "POST" => c.post(&url).json(&f["request"]),
            "GET" => c.get(&url),
            _ => unreachable!(),
        };
        let resp = req.send().await.unwrap();
        assert_eq!(
            resp.status().as_u16(),
            f["status"].as_u64().unwrap() as u16,
            "{name}"
        );
        let bytes = resp.bytes().await.unwrap();
        match f.get("raw") {
            Some(raw) => assert_eq!(bytes.to_vec(), b64(raw.as_str().unwrap()), "{name}"),
            None => assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap(),
                f["response"],
                "{name}"
            ),
        }
    }
}

#[tokio::test]
async fn the_host_sends_every_plugin_to_daemon_fixture_and_reads_the_answer() {
    let fx = fixtures();
    let record: FleetRecord = serde_json::from_value(fx["fleets"]["response"][0].clone()).unwrap();
    let fake = FakeHost::start("tok", json!({ "greeting": "hi" }), vec![record]).await;
    let host = Host::new(fake.env("flow", Path::new("/s"))).unwrap();
    // A raw client for asserting a fixture's exact recorded status and body
    // beyond what the typed `Host` API surfaces (e.g. `Result<(), _>` calls
    // collapse a 200 and its `{}` body to `Ok(())`; `SdkError::Status` keeps
    // only the message, not the numeric code as read from the fixture).
    let c = reqwest::Client::builder().no_proxy().build().unwrap();

    let r = host.hello("0.1.0", "127.0.0.1:4000").await.unwrap();
    assert_eq!(serde_json::to_value(&r).unwrap(), fx["hello"]["response"]);
    assert_eq!(
        serde_json::to_value(&fake.hellos()[0]).unwrap(),
        fx["hello"]["request"]
    );
    let mut env = fake.env("flow", Path::new("/s"));
    env.token = fx["hello-bad-token"]["token"].as_str().unwrap().into();
    let e = Host::new(env)
        .unwrap()
        .hello("0.1.0", "127.0.0.1:4000")
        .await
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        format!(
            "daemon: HTTP 401: {}",
            fx["hello-bad-token"]["response"]["error"].as_str().unwrap()
        )
    );
    // `Display` above only proves the message; check the fixture's exact
    // recorded status and body on the wire.
    let resp = c
        .post(format!("{}/v1/plugin-host/hello", fake.url))
        .bearer_auth(fx["hello-bad-token"]["token"].as_str().unwrap())
        .json(&fx["hello-bad-token"]["request"])
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        fx["hello-bad-token"]["status"].as_u64().unwrap() as u16
    );
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        fx["hello-bad-token"]["response"]
    );

    let fleets = host.fleets().await.unwrap();
    assert_eq!(
        serde_json::to_value(&fleets).unwrap(),
        fx["fleets"]["response"]
    );
    assert_eq!(host.fleet("nope").await.unwrap(), None);
    // `Host::fleet` collapses every non-2xx to `None`; check the fixture's
    // exact recorded status and body on the wire.
    let resp = c
        .get(format!("{}/v1/plugin-host/fleets/nope", fake.url))
        .bearer_auth("tok")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        fx["fleet-missing"]["status"].as_u64().unwrap() as u16
    );
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        fx["fleet-missing"]["response"]
    );

    let action: PluginAction = serde_json::from_value(fx["action"]["request"].clone()).unwrap();
    host.action("payments/backend/bob", &action).await.unwrap();
    assert_eq!(
        fake.actions()[0],
        ("payments/backend/bob".to_string(), action)
    );

    let bytes = b64(fx["kv-put"]["raw"].as_str().unwrap());
    host.kv_put("state/payments/backend/bob", &bytes, false)
        .await
        .unwrap();
    assert_eq!(
        host.kv_get("state/payments/backend/bob").await.unwrap(),
        Some(b64(fx["kv-get"]["raw"].as_str().unwrap()))
    );
    assert_eq!(
        serde_json::to_value(host.kv_list("state/").await.unwrap()).unwrap(),
        fx["kv-list"]["response"]["keys"]
    );
}
