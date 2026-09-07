//! `FakeHost`: an axum app speaking the daemon's plugin-host wire format
//! (plugins spec §4.1) exactly enough for plugin unit tests to exercise a
//! real `Host` against it — no capability gating, no real fleets; `Harness`:
//! the plugin side, driving a `Plugin` through the real §4.2 router (plugins
//! spec §17.5). `FakeHost` is the reference the daemon's conformance tests
//! replay against, so its bodies must match the real daemon's.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use hecaton_api::{
    CHAIN_BUDGET_MS, ErrorBody, FleetRecord, HelloRequest, HelloResponse, HookEvent,
    InterceptRequest, InterceptResponse, KvKeys, PluginAction, Timestamp,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Env, Host, Plugin, SdkError, bind, run};

struct Inner {
    token: String,
    config: Value,
    fleets: Vec<FleetRecord>,
    hellos: Mutex<Vec<HelloRequest>>,
    actions: Mutex<Vec<(String, PluginAction)>>,
    kv: Mutex<BTreeMap<String, (Vec<u8>, bool)>>,
}

/// A fake daemon, started on `127.0.0.1:0`, that a real `Host` can talk to.
pub struct FakeHost {
    pub url: String,
    inner: Arc<Inner>,
}

impl FakeHost {
    pub async fn start(token: &str, config: Value, fleets: Vec<FleetRecord>) -> FakeHost {
        let inner = Arc::new(Inner {
            token: token.to_string(),
            config,
            fleets,
            hellos: Mutex::new(Vec::new()),
            actions: Mutex::new(Vec::new()),
            kv: Mutex::new(BTreeMap::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("FakeHost bind: {e}"));
        let addr = listener
            .local_addr()
            .unwrap_or_else(|e| panic!("FakeHost local_addr: {e}"));
        let app = router(inner.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeHost {
            url: format!("http://{addr}"),
            inner,
        }
    }

    pub fn env(&self, name: &str, scratch: &Path) -> Env {
        Env {
            api_url: self.url.clone(),
            name: name.to_string(),
            token: self.inner.token.clone(),
            scratch: scratch.into(),
        }
    }

    pub fn hellos(&self) -> Vec<HelloRequest> {
        self.inner
            .hellos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn actions(&self) -> Vec<(String, PluginAction)> {
        self.inner
            .actions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn kv(&self) -> BTreeMap<String, (Vec<u8>, bool)> {
        self.inner
            .kv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// A kv entry parsed as JSON; `None` if absent or not JSON.
    pub fn kv_json(&self, key: &str) -> Option<Value> {
        self.kv()
            .get(key)
            .and_then(|(bytes, _)| serde_json::from_slice(bytes).ok())
    }

    /// The actions posted for one agent, in order.
    pub fn actions_for(&self, agent: &str) -> Vec<PluginAction> {
        self.actions()
            .into_iter()
            .filter(|(a, _)| a == agent)
            .map(|(_, action)| action)
            .collect()
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: message.into(),
        }),
    )
        .into_response()
}

/// `None` if the bearer matches; otherwise the 401 to return.
fn unauthorized(inner: &Inner, headers: &HeaderMap) -> Option<Response> {
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|tok| tok == inner.token);
    if ok {
        None
    } else {
        Some(error(
            StatusCode::UNAUTHORIZED,
            "unknown plugin or bad token",
        ))
    }
}

fn router(inner: Arc<Inner>) -> Router {
    Router::new()
        .route("/v1/plugin-host/hello", axum::routing::post(hello))
        .route("/v1/plugin-host/fleets", get(fleets))
        .route("/v1/plugin-host/fleets/{name}", get(fleet))
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/actions",
            axum::routing::post(post_action),
        )
        .route("/v1/plugin-host/kv", get(list_keys))
        .route(
            "/v1/plugin-host/kv/{*key}",
            get(get_key).put(put_key).delete(delete_key),
        )
        .with_state(inner)
}

async fn hello(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    body: Result<Json<HelloRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Some(resp) = unauthorized(&inner, &headers) {
        return resp;
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    inner
        .hellos
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(req);
    Json(HelloResponse {
        config: inner.config.clone(),
    })
    .into_response()
}

async fn fleets(State(inner): State<Arc<Inner>>, headers: HeaderMap) -> Response {
    if let Some(resp) = unauthorized(&inner, &headers) {
        return resp;
    }
    Json(inner.fleets.clone()).into_response()
}

async fn fleet(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Response {
    if let Some(resp) = unauthorized(&inner, &headers) {
        return resp;
    }
    match inner.fleets.iter().find(|f| f.name() == name) {
        Some(f) => Json(f.clone()).into_response(),
        None => error(StatusCode::NOT_FOUND, "fleet not found"),
    }
}

async fn post_action(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    AxumPath((f, c, a)): AxumPath<(String, String, String)>,
    body: Result<Json<PluginAction>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Some(resp) = unauthorized(&inner, &headers) {
        return resp;
    }
    let Json(action) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    inner
        .actions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((format!("{f}/{c}/{a}"), action));
    Json(json!({})).into_response()
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct KvQuery {
    prefix: String,
    secret: bool,
}

async fn list_keys(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    Query(q): Query<KvQuery>,
) -> Response {
    if let Some(resp) = unauthorized(&inner, &headers) {
        return resp;
    }
    let keys = inner
        .kv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .filter(|k| k.starts_with(&q.prefix))
        .cloned()
        .collect();
    Json(KvKeys { keys }).into_response()
}

async fn get_key(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    AxumPath(key): AxumPath<String>,
) -> Response {
    if let Some(resp) = unauthorized(&inner, &headers) {
        return resp;
    }
    match inner
        .kv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned()
    {
        Some((bytes, _)) => ([(CONTENT_TYPE, "application/octet-stream")], bytes).into_response(),
        None => error(StatusCode::NOT_FOUND, "no such key"),
    }
}

async fn put_key(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    AxumPath(key): AxumPath<String>,
    Query(q): Query<KvQuery>,
    body: Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    if let Some(resp) = unauthorized(&inner, &headers) {
        return resp;
    }
    let body = match body {
        Ok(b) => b,
        Err(e) => return error(e.status(), e.body_text()),
    };
    inner
        .kv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, (body.to_vec(), q.secret));
    Json(json!({})).into_response()
}

async fn delete_key(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    AxumPath(key): AxumPath<String>,
) -> Response {
    if let Some(resp) = unauthorized(&inner, &headers) {
        return resp;
    }
    inner
        .kv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    Json(json!({})).into_response()
}

/// A `HookEvent` with the boilerplate filled: `session_id` "test",
/// `received_at` 1.
pub fn event(agent: &str, name: &str, payload: Value) -> HookEvent {
    HookEvent {
        agent: agent.to_string(),
        name: name.to_string(),
        session_id: Some("test".into()),
        received_at: Timestamp(1),
        payload,
    }
}

/// The value of the first sample of `family` whose labels include every
/// `(name, value)` given — a small reader of the Prometheus text format
/// so a test asserts one number, not a substring.
pub fn metric(text: &str, family: &str, labels: &[(&str, &str)]) -> Option<f64> {
    text.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with('#') {
            return None;
        }
        let (name, rest) = match line.find(['{', ' ']) {
            Some(i) => (&line[..i], &line[i..]),
            None => return None,
        };
        if name != family {
            return None;
        }
        let (label_text, value) = match rest.strip_prefix('{') {
            Some(r) => {
                let end = r.find('}')?;
                (&r[..end], r[end + 1..].trim())
            }
            None => ("", rest.trim()),
        };
        let have: Vec<(&str, &str)> = label_text
            .split(',')
            .filter(|p| !p.is_empty())
            .filter_map(|p| {
                let (k, v) = p.split_once('=')?;
                Some((k, v.trim_matches('"')))
            })
            .collect();
        if labels.iter().all(|want| have.contains(want)) {
            value.parse().ok()
        } else {
            None
        }
    })
}

/// A plugin served through the real router, spoken to over HTTP.
pub struct Harness {
    host: Host,
    http: reqwest::Client,
    listen: String,
    token: String,
    server: tokio::task::JoinHandle<Result<(), SdkError>>,
}

impl Harness {
    /// Binds a loopback port, serves `plugin`'s router on it and sends
    /// `hello` (version "test") to the `FakeHost` behind `env`.
    pub async fn start<P: Plugin>(env: &Env, plugin: P) -> Harness {
        let host = Host::new(env.clone()).unwrap_or_else(|e| panic!("Harness host: {e}"));
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap_or_else(|e| panic!("Harness client: {e}"));
        let token = env.token.clone();
        let (listen, server) = Self::spawn(&host, plugin, &token).await;
        Harness {
            host,
            http,
            listen,
            token,
            server,
        }
    }

    async fn spawn<P: Plugin>(
        host: &Host,
        plugin: P,
        token: &str,
    ) -> (String, tokio::task::JoinHandle<Result<(), SdkError>>) {
        let (listener, listen) = bind().await.unwrap_or_else(|e| panic!("Harness bind: {e}"));
        let plugin = Arc::new(plugin);
        let token = token.to_string();
        let server = tokio::spawn(async move { run(listener, plugin, &token).await });
        host.hello("test", &listen)
            .await
            .unwrap_or_else(|e| panic!("Harness hello: {e}"));
        (listen, server)
    }

    pub fn listen(&self) -> &str {
        &self.listen
    }

    /// The bearer every Harness call sends: the token the plugin was
    /// started with.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Stops the served instance and serves `plugin` in its place, with
    /// a fresh `hello`: the "plugin restarted" case. The `FakeHost` and
    /// its kv are untouched.
    pub async fn restart<P: Plugin>(&mut self, plugin: P) {
        self.server.abort();
        let _ = (&mut self.server).await;
        let (listen, server) = Self::spawn(&self.host, plugin, &self.token).await;
        self.listen = listen;
        self.server = server;
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.listen)
    }

    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("Harness POST {path}: {e}"));
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    fn message(body: &Value) -> String {
        body["error"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| body.to_string())
    }

    pub async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        let (status, body) = self
            .post("/v1/activate", &json!({ "agent": agent, "config": config }))
            .await;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(Self::message(&body))
        }
    }

    pub async fn deactivate(&self, agent: &str) {
        let (status, body) = self
            .post("/v1/deactivate", &json!({ "agent": agent }))
            .await;
        assert_eq!(status, 200, "deactivate: {body}");
    }

    pub async fn observe(&self, events: Vec<HookEvent>) {
        let (status, body) = self.post("/v1/events", &json!({ "events": events })).await;
        assert_eq!(status, 200, "events: {body}");
    }

    /// `intercept` with `response_so_far` `{}` and the full chain budget.
    pub async fn intercept(&self, event: HookEvent) -> InterceptResponse {
        self.intercept_with(event, json!({}), CHAIN_BUDGET_MS).await
    }

    pub async fn intercept_with(
        &self,
        event: HookEvent,
        so_far: Value,
        deadline_ms: u64,
    ) -> InterceptResponse {
        let req = InterceptRequest {
            event,
            response_so_far: so_far,
            deadline_ms,
        };
        let body = serde_json::to_value(&req).unwrap_or_else(|e| panic!("intercept body: {e}"));
        let (status, body) = self.post("/v1/intercept", &body).await;
        assert_eq!(status, 200, "intercept: {body}");
        serde_json::from_value(body).unwrap_or_else(|e| panic!("intercept reply: {e}"))
    }

    pub async fn health(&self) -> Result<(), String> {
        let resp = self
            .http
            .get(self.url("/v1/health"))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap_or_else(|e| panic!("Harness GET health: {e}"));
        if resp.status().is_success() {
            Ok(())
        } else {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            Err(Self::message(&body))
        }
    }

    pub async fn metrics(&self) -> String {
        self.http
            .get(self.url("/v1/metrics"))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap_or_else(|e| panic!("Harness GET metrics: {e}"))
            .text()
            .await
            .unwrap_or_default()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Metrics, Plugin};
    use hecaton_api::{HookEvent, InterceptResponse, PluginAction};

    /// Counts intercepts and observed events in gauges; rejects agents
    /// ending in `/bad`; health as constructed.
    struct Counting {
        metrics: Metrics,
        intercepts: crate::metrics::IntGauge,
        observed: crate::metrics::IntGauge,
        healthy: bool,
    }

    impl Counting {
        fn new(healthy: bool) -> Self {
            let metrics = Metrics::new("cnt");
            let intercepts = metrics.int_gauge("intercepts", "n").unwrap();
            let observed = metrics.int_gauge("observed", "n").unwrap();
            Self {
                metrics,
                intercepts,
                observed,
                healthy,
            }
        }
    }

    impl Plugin for Counting {
        async fn activate(&self, agent: &str, _config: Value) -> Result<(), String> {
            if agent.ends_with("/bad") {
                return Err("initial: no state \"x\" declared".into());
            }
            Ok(())
        }
        async fn observe(&self, events: Vec<HookEvent>) {
            self.observed.add(events.len() as i64);
        }
        async fn intercept(
            &self,
            event: HookEvent,
            mut so_far: Value,
            deadline_ms: u64,
        ) -> InterceptResponse {
            self.intercepts.inc();
            so_far["seen"] = json!(event.name);
            so_far["deadline"] = json!(deadline_ms);
            InterceptResponse {
                response: so_far,
                actions: vec![PluginAction::Stop],
            }
        }
        async fn health(&self) -> Result<(), String> {
            if self.healthy {
                Ok(())
            } else {
                Err("warming up".into())
            }
        }
        fn metrics(&self) -> Option<&Metrics> {
            Some(&self.metrics)
        }
    }

    #[tokio::test]
    async fn the_harness_drives_every_route_through_the_wire() {
        let fake = FakeHost::start("tok", json!({}), vec![]).await;
        let env = fake.env("cnt", Path::new("/s"));
        let mut h = Harness::start(&env, Counting::new(false)).await;
        assert!(h.listen().starts_with("127.0.0.1:"));
        assert_eq!(fake.hellos().len(), 1, "start said hello");
        assert_eq!(fake.hellos()[0].listen, h.listen());

        assert_eq!(h.activate("f/c/a", json!({ "k": 1 })).await, Ok(()));
        assert_eq!(
            h.activate("f/c/bad", json!({})).await,
            Err("initial: no state \"x\" declared".into())
        );
        // a malformed body (no `config`) never reaches the plugin: the
        // extractor itself answers 400 (plugin-protocol §4.2)
        let (status, body) = h.post("/v1/activate", &json!({ "agent": "f/c/a" })).await;
        assert_eq!(status, 400, "{body}");
        h.deactivate("f/c/a").await;
        h.observe(vec![event("f/c/a", "SessionStart", json!({}))])
            .await;
        let v = h
            .intercept(event("f/c/a", "PreToolUse", json!({ "tool_name": "Bash" })))
            .await;
        assert_eq!(
            v.response,
            json!({ "seen": "PreToolUse", "deadline": 1500 })
        );
        assert_eq!(v.actions, vec![PluginAction::Stop]);
        let v = h
            .intercept_with(event("f/c/a", "Stop", json!({})), json!({ "a": 1 }), 7)
            .await;
        assert_eq!(v.response, json!({ "a": 1, "seen": "Stop", "deadline": 7 }));
        assert_eq!(h.health().await, Err("warming up".into()));
        let text = h.metrics().await;
        assert_eq!(
            metric(&text, "hecaton_plugin_cnt_intercepts", &[]),
            Some(2.0)
        );
        assert_eq!(metric(&text, "hecaton_plugin_cnt_observed", &[]), Some(1.0));

        // restart: a new instance, the same FakeHost, a second hello
        h.restart(Counting::new(true)).await;
        assert_eq!(fake.hellos().len(), 2);
        assert_eq!(h.health().await, Ok(()));
        assert_eq!(
            metric(&h.metrics().await, "hecaton_plugin_cnt_intercepts", &[]),
            Some(0.0),
            "a fresh instance starts from zero"
        );
    }

    #[test]
    fn event_fills_the_boilerplate_and_metric_parses_the_text_format() {
        let e = event("f/c/a", "PreToolUse", json!({ "x": 1 }));
        assert_eq!(e.agent, "f/c/a");
        assert_eq!(e.name, "PreToolUse");
        assert_eq!(e.session_id.as_deref(), Some("test"));
        assert_eq!(e.payload["x"], 1);

        let text = "# HELP hecaton_plugin_flow_state s\n# TYPE hecaton_plugin_flow_state gauge\nhecaton_plugin_flow_state{agent=\"a\",fleet=\"f\",state=\"working\"} 1\nhecaton_plugin_flow_state{agent=\"b\",fleet=\"f\",state=\"done\"} 1\nhecaton_plugin_flow_up 1\nhecaton_plugin_flow_ratio 0.5\n";
        assert_eq!(
            metric(text, "hecaton_plugin_flow_state", &[("agent", "a")]),
            Some(1.0)
        );
        assert_eq!(
            metric(
                text,
                "hecaton_plugin_flow_state",
                &[("agent", "b"), ("state", "done")]
            ),
            Some(1.0)
        );
        assert_eq!(
            metric(
                text,
                "hecaton_plugin_flow_state",
                &[("agent", "b"), ("state", "working")]
            ),
            None
        );
        assert_eq!(metric(text, "hecaton_plugin_flow_up", &[]), Some(1.0));
        assert_eq!(metric(text, "hecaton_plugin_flow_ratio", &[]), Some(0.5));
        assert_eq!(metric(text, "hecaton_plugin_flow_nope", &[]), None);
        assert_eq!(
            metric(text, "hecaton_plugin_flow_state", &[]),
            Some(1.0),
            "no labels given: the first sample of the family"
        );
    }

    #[tokio::test]
    async fn fake_host_helpers_read_kv_as_json_and_actions_per_agent() {
        let fake = FakeHost::start("tok", json!({}), vec![]).await;
        let host = crate::Host::new(fake.env("p", Path::new("/s"))).unwrap();
        host.kv_put("state/f/c/a", br#"{"state":"working"}"#, false)
            .await
            .unwrap();
        host.kv_put("blob", b"\xff\xfe", false).await.unwrap();
        assert_eq!(
            fake.kv_json("state/f/c/a"),
            Some(json!({ "state": "working" }))
        );
        assert_eq!(fake.kv_json("blob"), None, "not JSON");
        assert_eq!(fake.kv_json("missing"), None);
        host.action("f/c/a", &PluginAction::Stop).await.unwrap();
        host.action("f/c/b", &PluginAction::Restart).await.unwrap();
        host.action(
            "f/c/a",
            &PluginAction::SendText {
                text: "hi".into(),
                submit: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            fake.actions_for("f/c/a"),
            vec![
                PluginAction::Stop,
                PluginAction::SendText {
                    text: "hi".into(),
                    submit: true
                }
            ]
        );
        assert_eq!(fake.actions_for("f/c/b"), vec![PluginAction::Restart]);
        assert!(fake.actions_for("f/c/zz").is_empty());
    }
}
