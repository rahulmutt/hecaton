//! The daemon → plugin half (plugins spec §4.2, §7): implement `Plugin`,
//! hand it to `serve`. Every method has a no-op default so a plugin
//! implements only what it subscribes to.

use std::future::{Future, IntoFuture};
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hecaton_api::{
    ActivateRequest, DeactivateRequest, ErrorBody, EventBatch, HookEvent, InterceptRequest,
    InterceptResponse,
};
use serde_json::{Value, json};

use crate::{Host, SdkError};

pub trait Plugin: Send + Sync + 'static {
    /// `activate`: `Err(message)` rejects the agent's config; the daemon
    /// reports it as `crews.<c>.agents.<a>.plugins.<name>: <message>`.
    fn activate(
        &self,
        agent: &str,
        config: Value,
    ) -> impl Future<Output = Result<(), String>> + Send {
        let _ = (agent, config);
        async { Ok(()) }
    }
    fn deactivate(&self, agent: &str) -> impl Future<Output = ()> + Send {
        let _ = agent;
        async {}
    }
    fn observe(&self, events: Vec<HookEvent>) -> impl Future<Output = ()> + Send {
        let _ = events;
        async {}
    }
    /// The verdict; default passes `response_so_far` through untouched.
    fn intercept(
        &self,
        event: HookEvent,
        response_so_far: Value,
        deadline_ms: u64,
    ) -> impl Future<Output = InterceptResponse> + Send {
        let _ = (event, deadline_ms);
        async move {
            InterceptResponse {
                response: response_so_far,
                actions: Vec::new(),
            }
        }
    }
    fn health(&self) -> impl Future<Output = Result<(), String>> + Send {
        async { Ok(()) }
    }
    /// Prometheus text; every family must start with `hecaton_plugin_<name>_`.
    fn metrics(&self) -> impl Future<Output = String> + Send {
        async { String::new() }
    }
}

pub fn router<P: Plugin>(plugin: Arc<P>) -> Router {
    Router::new()
        .route("/v1/activate", post(activate::<P>))
        .route("/v1/deactivate", post(deactivate::<P>))
        .route("/v1/events", post(events::<P>))
        .route("/v1/intercept", post(intercept::<P>))
        .route("/v1/health", get(health::<P>))
        .route("/v1/metrics", get(metrics::<P>))
        .with_state(plugin)
        // daemon → plugin request bodies are capped at 1 MiB (plugin-protocol
        // §1), matching the daemon's own `plugin_api::router` layer
        // (`hecaton-server/src/api.rs`); axum's default (2 MiB) is otherwise
        // silently more permissive than the spec promises.
        .layer(DefaultBodyLimit::max(1 << 20))
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

async fn activate<P: Plugin>(
    State(p): State<Arc<P>>,
    body: Result<Json<ActivateRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    match p.activate(&req.agent, req.config).await {
        Ok(()) => Json(json!({})).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn deactivate<P: Plugin>(
    State(p): State<Arc<P>>,
    body: Result<Json<DeactivateRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    p.deactivate(&req.agent).await;
    Json(json!({})).into_response()
}

async fn events<P: Plugin>(
    State(p): State<Arc<P>>,
    body: Result<Json<EventBatch>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(batch) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    p.observe(batch.events).await;
    Json(json!({})).into_response()
}

async fn intercept<P: Plugin>(
    State(p): State<Arc<P>>,
    body: Result<Json<InterceptRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    let verdict = p
        .intercept(req.event, req.response_so_far, req.deadline_ms)
        .await;
    Json(verdict).into_response()
}

async fn health<P: Plugin>(State(p): State<Arc<P>>) -> Response {
    match p.health().await {
        Ok(()) => (StatusCode::OK, "ok").into_response(),
        Err(message) => error(StatusCode::SERVICE_UNAVAILABLE, message),
    }
}

async fn metrics<P: Plugin>(State(p): State<Arc<P>>) -> Response {
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4")],
        p.metrics().await,
    )
        .into_response()
}

/// A loopback listener on an ephemeral port and its `host:port`.
pub async fn bind() -> Result<(tokio::net::TcpListener, String), SdkError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| SdkError::Bind(e.to_string()))?;
    let listen = listener
        .local_addr()
        .map_err(|e| SdkError::Bind(e.to_string()))?
        .to_string();
    Ok((listener, listen))
}

/// Serves the router until the future is dropped.
pub async fn run<P: Plugin>(
    listener: tokio::net::TcpListener,
    plugin: Arc<P>,
) -> Result<(), SdkError> {
    axum::serve(listener, router(plugin))
        .into_future()
        .await
        .map_err(|e| SdkError::Bind(e.to_string()))
}

/// Bind, say hello, serve. Returns only on a bind or hello failure, or
/// when the server stops.
pub async fn serve<P: Plugin>(host: &Host, version: &str, plugin: P) -> Result<(), SdkError> {
    let (listener, listen) = bind().await?;
    serve_on(host, version, plugin, listener, listen).await
}

/// `serve`'s body, taking an already-bound listener so tests can observe
/// its address. If `hello` fails, the spawned server is aborted (and
/// awaited, so the port is free again) before the error is returned — a
/// dropped `JoinHandle` alone would only detach the task, leaking the
/// listener and leaving `axum::serve` running forever.
async fn serve_on<P: Plugin>(
    host: &Host,
    version: &str,
    plugin: P,
    listener: tokio::net::TcpListener,
    listen: String,
) -> Result<(), SdkError> {
    let plugin = Arc::new(plugin);
    let server = tokio::spawn(run(listener, plugin));
    if let Err(e) = host.hello(version, &listen).await {
        server.abort();
        let _ = server.await;
        return Err(e);
    }
    server.await.map_err(|e| SdkError::Bind(e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use hecaton_api::{HookEvent, PluginAction, Timestamp};
    use serde_json::{Value, json};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        activated: Mutex<Vec<(String, Value)>>,
        deactivated: Mutex<Vec<String>>,
        observed: Mutex<Vec<HookEvent>>,
        healthy: Mutex<bool>,
    }

    impl Plugin for Recorder {
        async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
            if agent.ends_with("/bad") {
                return Err("states.working.on[1].match: bad regex".into());
            }
            self.activated
                .lock()
                .unwrap()
                .push((agent.to_string(), config));
            Ok(())
        }
        async fn deactivate(&self, agent: &str) {
            self.deactivated.lock().unwrap().push(agent.to_string());
        }
        async fn observe(&self, events: Vec<HookEvent>) {
            self.observed.lock().unwrap().extend(events);
        }
        async fn intercept(
            &self,
            event: HookEvent,
            mut response_so_far: Value,
            deadline_ms: u64,
        ) -> InterceptResponse {
            response_so_far["seen"] = json!(event.name);
            response_so_far["deadline"] = json!(deadline_ms);
            InterceptResponse {
                response: response_so_far,
                actions: vec![PluginAction::Stop],
            }
        }
        async fn health(&self) -> Result<(), String> {
            if *self.healthy.lock().unwrap() {
                Ok(())
            } else {
                Err("warming up".into())
            }
        }
        async fn metrics(&self) -> String {
            "hecaton_plugin_rec_up 1\n".into()
        }
    }

    /// A plugin with every method left at its default.
    struct Silent;
    impl Plugin for Silent {}

    async fn post(url: &str, body: Value) -> (u16, Value) {
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        let r = c.post(url).json(&body).send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    #[tokio::test]
    async fn the_router_speaks_section_4_2() {
        let plugin = Arc::new(Recorder::default());
        let (listener, listen) = bind().await.unwrap();
        tokio::spawn(run(listener, plugin.clone()));
        let base = format!("http://{listen}");
        let (s, v) = post(
            &format!("{base}/v1/activate"),
            json!({ "agent": "f/c/a", "config": { "k": 1 } }),
        )
        .await;
        assert_eq!((s, v), (200, json!({})));
        let (s, v) = post(
            &format!("{base}/v1/activate"),
            json!({ "agent": "f/c/bad", "config": {} }),
        )
        .await;
        assert_eq!(s, 400);
        assert_eq!(v["error"], "states.working.on[1].match: bad regex");
        assert_eq!(plugin.activated.lock().unwrap()[0].0, "f/c/a");
        let (s, _) = post(
            &format!("{base}/v1/deactivate"),
            json!({ "agent": "f/c/a" }),
        )
        .await;
        assert_eq!(s, 200);
        let event = HookEvent {
            agent: "f/c/a".into(),
            name: "PreToolUse".into(),
            session_id: None,
            received_at: Timestamp(1),
            payload: json!({}),
        };
        let (s, _) = post(&format!("{base}/v1/events"), json!({ "events": [event] })).await;
        assert_eq!(s, 200);
        assert_eq!(plugin.observed.lock().unwrap().len(), 1);
        let (s, v) = post(
            &format!("{base}/v1/intercept"),
            json!({ "event": event, "response_so_far": { "a": 1 }, "deadline_ms": 900 }),
        )
        .await;
        assert_eq!(s, 200);
        assert_eq!(
            v["response"],
            json!({ "a": 1, "seen": "PreToolUse", "deadline": 900 })
        );
        assert_eq!(v["actions"], json!([{ "action": "stop" }]));
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            c.get(format!("{base}/v1/health"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            503
        );
        *plugin.healthy.lock().unwrap() = true;
        assert_eq!(
            c.get(format!("{base}/v1/health"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            200
        );
        let m = c.get(format!("{base}/v1/metrics")).send().await.unwrap();
        assert!(
            m.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/plain")
        );
        assert_eq!(m.text().await.unwrap(), "hecaton_plugin_rec_up 1\n");
        let (s, v) = post(&format!("{base}/v1/activate"), json!({ "agent": "f/c/a" })).await;
        assert_eq!(s, 400, "{v}");
    }

    #[tokio::test]
    async fn defaults_accept_everything_and_pass_the_response_through() {
        let (listener, listen) = bind().await.unwrap();
        tokio::spawn(run(listener, Arc::new(Silent)));
        let base = format!("http://{listen}");
        let (s, _) = post(
            &format!("{base}/v1/activate"),
            json!({ "agent": "f/c/a", "config": {} }),
        )
        .await;
        assert_eq!(s, 200);
        let event = HookEvent {
            agent: "f/c/a".into(),
            name: "Stop".into(),
            session_id: None,
            received_at: Timestamp(1),
            payload: json!({}),
        };
        let (s, v) = post(
            &format!("{base}/v1/intercept"),
            json!({ "event": event, "response_so_far": { "x": 2 }, "deadline_ms": 5 }),
        )
        .await;
        assert_eq!((s, v), (200, json!({ "response": { "x": 2 } })));
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            c.get(format!("{base}/v1/health"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            200
        );
        assert_eq!(
            c.get(format!("{base}/v1/metrics"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn serve_binds_says_hello_and_runs() {
        let fake = crate::testing::FakeHost::start("tok", json!({}), vec![]).await;
        let host = Host::new(fake.env("rec", std::path::Path::new("/s"))).unwrap();
        let handle = tokio::spawn(async move { serve(&host, "0.1.0", Silent).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while fake.hellos().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let listen = fake.hellos()[0].listen.clone();
        assert!(listen.starts_with("127.0.0.1:"));
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            c.get(format!("http://{listen}/v1/health"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            200
        );
        handle.abort();
    }

    /// serve() must not leak the spawned server (and its listener) when
    /// `hello` fails: dropping a `JoinHandle` only detaches the task, it
    /// does not cancel it.
    #[tokio::test]
    async fn a_failed_hello_stops_the_server_and_frees_the_port() {
        let fake = crate::testing::FakeHost::start("tok", json!({}), vec![]).await;
        let mut env = fake.env("rec", std::path::Path::new("/s"));
        env.token = "wrong".into();
        let host = Host::new(env).unwrap();
        let (listener, listen) = bind().await.unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            serve_on(&host, "0.1.0", Silent, listener, listen.clone()),
        )
        .await
        .unwrap();
        assert!(
            matches!(&result, Err(SdkError::Status { status: 401, .. })),
            "{result:?}"
        );
        assert!(fake.hellos().is_empty(), "the daemon never saw a hello");

        // The abort is asynchronous: the aborted task's future (and the
        // listener it owns) is dropped on the runtime's next poll, not
        // synchronously inside `abort()`. Retry briefly rather than
        // asserting on the first attempt.
        let freed = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if tokio::net::TcpListener::bind(&listen).await.is_ok() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            freed.is_ok(),
            "port {listen} not freed after the failed hello"
        );
    }
}
