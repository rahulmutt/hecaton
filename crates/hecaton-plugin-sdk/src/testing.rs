//! `FakeHost`: an axum app speaking the daemon's plugin-host wire format
//! (plugins spec §4.1) exactly enough for plugin unit tests to exercise a
//! real `Host` against it — no capability gating, no real fleets. It is
//! the reference the daemon's conformance tests replay against, so its
//! bodies must match the real daemon's.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use hecaton_api::{ErrorBody, FleetRecord, HelloRequest, HelloResponse, KvKeys, PluginAction};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Env;

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
