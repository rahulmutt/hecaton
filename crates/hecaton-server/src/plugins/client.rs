//! Every daemon → plugin call (plugins spec §4.2) on one `reqwest` client:
//! loopback, plain HTTP/1.1, no proxy, 5 s unless the caller says otherwise.
//! Failures are classified into the four `reason` labels of §4.3.

use std::fmt;
use std::time::Duration;

use hecaton_api::{
    ActivateRequest, DeactivateRequest, ErrorBody, EventBatch, InterceptRequest, InterceptResponse,
};
use serde::Serialize;
use serde_json::Value;

use super::PluginError;

/// Default per-call timeout (§4.2 "5 s unless stated").
pub const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Plugin response bodies are capped like every other body.
const MAX_BODY: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallFailure {
    Timeout,
    Connect,
    Status { status: u16, message: String },
    Body(String),
}

impl CallFailure {
    /// The metrics `reason` label.
    pub fn reason(&self) -> &'static str {
        match self {
            CallFailure::Timeout => "timeout",
            CallFailure::Connect => "connect",
            CallFailure::Status { .. } => "status",
            CallFailure::Body(_) => "body",
        }
    }
}

impl fmt::Display for CallFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallFailure::Timeout => f.write_str("timeout"),
            CallFailure::Connect => f.write_str("connection refused"),
            CallFailure::Status { status, message } => write!(f, "HTTP {status}: {message}"),
            CallFailure::Body(m) => write!(f, "bad response body: {m}"),
        }
    }
}

impl From<reqwest::Error> for CallFailure {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            CallFailure::Timeout
        } else if e.is_connect() || e.is_request() {
            CallFailure::Connect
        } else {
            CallFailure::Body(e.to_string())
        }
    }
}

#[derive(Clone)]
pub struct PluginClient {
    http: reqwest::Client,
}

impl fmt::Debug for PluginClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PluginClient")
    }
}

impl PluginClient {
    pub fn new() -> Result<Self, PluginError> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(CALL_TIMEOUT)
            .build()
            .map_err(|e| PluginError::Internal(format!("http client: {e}")))?;
        Ok(Self { http })
    }

    fn url(listen: &str, path: &str) -> String {
        format!("http://{listen}{path}")
    }

    /// Reads at most `MAX_BODY` bytes; a non-2xx becomes `Status` with
    /// the `{ "error" }` message or the raw text.
    async fn body(resp: reqwest::Response) -> Result<Vec<u8>, CallFailure> {
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await?;
        if bytes.len() > MAX_BODY {
            return Err(CallFailure::Body(format!(
                "{} bytes exceeds 1 MiB",
                bytes.len()
            )));
        }
        if !(200..300).contains(&status) {
            let text = String::from_utf8_lossy(&bytes).trim().to_string();
            let message = serde_json::from_slice::<ErrorBody>(&bytes)
                .map(|e| e.error)
                .unwrap_or(text);
            return Err(CallFailure::Status { status, message });
        }
        Ok(bytes.to_vec())
    }

    async fn post<T: Serialize + ?Sized>(
        &self,
        listen: &str,
        path: &str,
        body: &T,
        timeout: Duration,
    ) -> Result<Vec<u8>, CallFailure> {
        let resp = self
            .http
            .post(Self::url(listen, path))
            .timeout(timeout)
            .json(body)
            .send()
            .await?;
        Self::body(resp).await
    }

    pub async fn activate(&self, listen: &str, req: &ActivateRequest) -> Result<(), CallFailure> {
        self.post(listen, "/v1/activate", req, CALL_TIMEOUT)
            .await
            .map(|_| ())
    }

    pub async fn deactivate(
        &self,
        listen: &str,
        req: &DeactivateRequest,
    ) -> Result<(), CallFailure> {
        self.post(listen, "/v1/deactivate", req, CALL_TIMEOUT)
            .await
            .map(|_| ())
    }

    pub async fn events(&self, listen: &str, batch: &EventBatch) -> Result<(), CallFailure> {
        self.post(listen, "/v1/events", batch, CALL_TIMEOUT)
            .await
            .map(|_| ())
    }

    /// `timeout` is what remains of the chain's budget. A `response` that
    /// is not a JSON object is a `Body` failure (§4.3).
    pub async fn intercept(
        &self,
        listen: &str,
        req: &InterceptRequest,
        timeout: Duration,
    ) -> Result<InterceptResponse, CallFailure> {
        let bytes = self.post(listen, "/v1/intercept", req, timeout).await?;
        let verdict: InterceptResponse =
            serde_json::from_slice(&bytes).map_err(|e| CallFailure::Body(e.to_string()))?;
        if !matches!(verdict.response, Value::Object(_)) {
            return Err(CallFailure::Body("response is not a JSON object".into()));
        }
        Ok(verdict)
    }

    pub async fn health(&self, listen: &str) -> Result<(), CallFailure> {
        let resp = self
            .http
            .get(Self::url(listen, "/v1/health"))
            .send()
            .await?;
        Self::body(resp).await.map(|_| ())
    }

    pub async fn metrics(&self, listen: &str, timeout: Duration) -> Result<String, CallFailure> {
        let resp = self
            .http
            .get(Self::url(listen, "/v1/metrics"))
            .timeout(timeout)
            .send()
            .await?;
        let bytes = Self::body(resp).await?;
        String::from_utf8(bytes).map_err(|e| CallFailure::Body(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use hecaton_api::{HookEvent, Timestamp};
    use serde_json::{Value, json};

    /// A plugin stub: activate rejects agent "bad", intercept echoes the
    /// event name (or returns a non-object for "PreCompact"), health is
    /// fine, metrics is one family, `/slow` never answers in time.
    async fn stub() -> String {
        let app = Router::new()
            .route(
                "/v1/activate",
                post(|Json(v): Json<Value>| async move {
                    if v["agent"] == "f/c/bad" {
                        (
                            axum::http::StatusCode::BAD_REQUEST,
                            Json(json!({ "error": "states.working: unknown event" })),
                        )
                    } else {
                        (axum::http::StatusCode::OK, Json(json!({})))
                    }
                }),
            )
            .route("/v1/deactivate", post(|| async { Json(json!({})) }))
            .route("/v1/events", post(|| async { Json(json!({})) }))
            .route(
                "/v1/intercept",
                post(|Json(v): Json<Value>| async move {
                    if v["event"]["name"] == "PreCompact" {
                        Json(json!({ "response": 7 }))
                    } else if v["event"]["name"] == "Stop" {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        Json(json!({ "response": {} }))
                    } else {
                        Json(json!({
                            "response": { "seen": v["event"]["name"], "so_far": v["response_so_far"] },
                            "actions": [{ "action": "stop" }]
                        }))
                    }
                }),
            )
            .route("/v1/health", get(|| async { "ok" }))
            .route(
                "/v1/metrics",
                get(|| async { "# TYPE hecaton_plugin_x_up gauge\nhecaton_plugin_x_up 1\n" }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    fn event(name: &str) -> HookEvent {
        HookEvent {
            agent: "f/c/a".into(),
            name: name.into(),
            session_id: None,
            received_at: Timestamp(1),
            payload: json!({}),
        }
    }

    #[tokio::test]
    async fn calls_reach_the_plugin_and_classify_failures() {
        let listen = stub().await;
        let c = PluginClient::new().unwrap();
        c.activate(
            &listen,
            &ActivateRequest {
                agent: "f/c/a".into(),
                config: json!({}),
            },
        )
        .await
        .unwrap();
        let e = c
            .activate(
                &listen,
                &ActivateRequest {
                    agent: "f/c/bad".into(),
                    config: json!({}),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            e,
            CallFailure::Status {
                status: 400,
                message: "states.working: unknown event".into()
            }
        );
        assert_eq!(e.reason(), "status");
        assert_eq!(e.to_string(), "HTTP 400: states.working: unknown event");
        c.deactivate(
            &listen,
            &DeactivateRequest {
                agent: "f/c/a".into(),
            },
        )
        .await
        .unwrap();
        c.events(
            &listen,
            &EventBatch {
                events: vec![event("Notification")],
            },
        )
        .await
        .unwrap();

        let v = c
            .intercept(
                &listen,
                &InterceptRequest {
                    event: event("PreToolUse"),
                    response_so_far: json!({ "a": 1 }),
                    deadline_ms: 1000,
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(v.response["seen"], "PreToolUse");
        assert_eq!(v.response["so_far"]["a"], 1);
        assert_eq!(v.actions, vec![hecaton_api::PluginAction::Stop]);
        let e = c
            .intercept(
                &listen,
                &InterceptRequest {
                    event: event("PreCompact"),
                    response_so_far: json!({}),
                    deadline_ms: 1000,
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert_eq!(e.reason(), "body");
        assert!(matches!(e, CallFailure::Body(_)), "{e}");
        let e = c
            .intercept(
                &listen,
                &InterceptRequest {
                    event: event("Stop"),
                    response_so_far: json!({}),
                    deadline_ms: 100,
                },
                Duration::from_millis(100),
            )
            .await
            .unwrap_err();
        assert_eq!(e, CallFailure::Timeout);
        let e = c.health("127.0.0.1:1").await.unwrap_err();
        assert_eq!(e, CallFailure::Connect);
        assert_eq!(e.reason(), "connect");
        c.health(&listen).await.unwrap();
        let text = c
            .metrics(&listen, Duration::from_millis(500))
            .await
            .unwrap();
        assert!(text.starts_with("# TYPE hecaton_plugin_x_up"));
    }
}
