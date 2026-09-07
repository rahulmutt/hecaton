//! The HTTP surface (Phase 3 spec §3.4): plain HTTP on loopback, admin
//! bearer on `/v1/fleets*`, per-agent secret on the events route.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use hecaton_api::{
    DownQuery, ErrorBody, FleetRequest, FleetSummary, HelloRequest, HelloResponse, PluginStatus,
    SyncReport,
};
use hecaton_core::{AgentName, FleetName, FleetRecord, Keep, NameError};
use serde_json::{Value, json};

use crate::auth::{RateLimiter, bearer, constant_time_eq};
use crate::daemon::{Daemon, DaemonError};
use crate::hooks;
use crate::plugins::PluginError;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) daemon: Arc<Daemon>,
    pub(crate) limiter: Arc<RateLimiter>,
}

/// Every error leaves as `{ "error": "<message>" }`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

impl From<DaemonError> for ApiError {
    fn from(e: DaemonError) -> Self {
        let status = match e {
            DaemonError::NotFound => StatusCode::NOT_FOUND,
            DaemonError::Conflict => StatusCode::CONFLICT,
            DaemonError::Invalid(_) => StatusCode::BAD_REQUEST,
            DaemonError::Unauthorized => StatusCode::UNAUTHORIZED,
            DaemonError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, e.to_string())
    }
}

impl From<PluginError> for ApiError {
    fn from(e: PluginError) -> Self {
        // A KV failure names the daemon's own on-disk path; the body goes
        // to a sandboxed plugin, which has no business learning it. The
        // operator gets the real one in the log.
        if let PluginError::Kv { path, message } = &e {
            tracing::warn!(path = %path.display(), "plugin kv storage error: {message}");
            return Self::new(StatusCode::INTERNAL_SERVER_ERROR, "kv: storage error");
        }
        let status = match &e {
            PluginError::Config { .. }
            | PluginError::Manifest(_)
            | PluginError::ManifestParse(_)
            | PluginError::Digest { .. }
            | PluginError::Package(_)
            | PluginError::StillDeclared(_)
            | PluginError::Activation { .. }
            | PluginError::KvKey(_) => StatusCode::BAD_REQUEST,
            PluginError::Fetch { .. } => StatusCode::BAD_GATEWAY,
            PluginError::Capability(_) => StatusCode::FORBIDDEN,
            PluginError::NotActive(_) => StatusCode::NOT_FOUND,
            PluginError::Io { .. } | PluginError::Internal(_) | PluginError::Kv { .. } => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        Self::new(status, e.to_string())
    }
}

/// 20 events/s with a burst of 50 per agent (spec §3.5).
const HOOK_RATE: f64 = 20.0;
const HOOK_BURST: f64 = 50.0;

pub fn router(daemon: Arc<Daemon>) -> Router {
    let state = AppState {
        daemon,
        limiter: Arc::new(RateLimiter::new(HOOK_RATE, HOOK_BURST)),
    };
    let admin = Router::new()
        .route("/v1/fleets", get(list_fleets).post(create_fleet))
        .route(
            "/v1/fleets/{name}",
            get(get_fleet).put(update_fleet).delete(delete_fleet),
        )
        .route("/v1/plugins", get(list_plugins))
        .route("/v1/plugins/sync", post(sync_plugins))
        .route("/v1/plugins/{name}", delete(purge_plugin))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin))
        .layer(DefaultBodyLimit::max(4 << 20));
    let agents = Router::new()
        .route(
            "/v1/agents/{fleet}/{crew}/{agent}/events",
            post(hooks::events),
        )
        .layer(DefaultBodyLimit::max(1 << 20));
    let plugins = Router::new()
        .route("/v1/plugin-host/hello", post(plugin_hello))
        .layer(DefaultBodyLimit::max(64 << 10));
    let plugin_host = crate::plugin_api::router().layer(DefaultBodyLimit::max(1 << 20));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .merge(admin)
        .merge(agents)
        .merge(plugins)
        .merge(plugin_host)
        .with_state(state)
}

/// Runs until `shutdown` resolves; in-flight requests finish.
pub async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

async fn require_admin(State(state): State<AppState>, req: Request, next: Next) -> Response {
    match bearer(req.headers()) {
        Some(t) if constant_time_eq(t.as_bytes(), state.daemon.token().as_bytes()) => {
            next.run(req).await
        }
        _ => ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid admin token")
            .into_response(),
    }
}

/// Per-plugin `/v1/metrics` scrape budget (plugins spec §9).
const SCRAPE_TIMEOUT: Duration = Duration::from_millis(500);

async fn metrics(State(state): State<AppState>) -> Response {
    let snapshots = state.daemon.snapshots().await;
    state.daemon.metrics().set_gauges(&snapshots);
    let mut body = state.daemon.metrics().encode();
    let registry = state.daemon.registry().clone();
    // The plugin name lives outside the spawned task (not inside its
    // returned value) so a panicked or cancelled scrape is still
    // attributable: a bare `JoinError` carries no payload to recover it
    // from. Every task is spawned before any is awaited, so this keeps
    // the scrapes running in parallel despite the sequential awaits below.
    let mut scrapes = Vec::new();
    for name in registry.names() {
        let Some(listen) = registry.ready_listen(&name) else {
            continue;
        };
        let client = state.daemon.client().clone();
        let handle = tokio::spawn(async move { client.metrics(&listen, SCRAPE_TIMEOUT).await });
        scrapes.push((name, handle));
    }
    for (name, handle) in scrapes {
        match handle.await {
            Ok(Ok(text)) if crate::plugin_api::families_ok(&text, name.as_str()) => {
                body.push_str(&text);
                if !text.ends_with('\n') {
                    body.push('\n');
                }
            }
            Ok(Ok(_)) => state.daemon.metrics().scrape_failure(name.as_str()),
            Ok(Err(e)) => {
                tracing::debug!(plugin = %name, "metrics scrape failed: {e}");
                state.daemon.metrics().scrape_failure(name.as_str());
            }
            Err(e) => {
                tracing::warn!(plugin = %name, "metrics scrape task failed: {e}");
                state.daemon.metrics().scrape_failure(name.as_str());
            }
        }
    }
    ([(CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

fn fleet_name(s: &str) -> Result<FleetName, ApiError> {
    s.parse()
        .map_err(|e: hecaton_core::NameError| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))
}

/// Malformed JSON and a body that doesn't match `FleetRequest` (including
/// an unknown field: `deny_unknown_fields`) are both a plain 400 — the
/// spec's error table only names 400 for a bad request body. Only the
/// rejections whose status carries information the client actually needs
/// pass through axum's own: a missing/wrong `Content-Type` (415) and a
/// body over the limit (413, via `BytesRejection`).
fn body(b: Result<Json<FleetRequest>, JsonRejection>) -> Result<FleetRequest, ApiError> {
    b.map(|Json(r)| r).map_err(|e| {
        let status = match &e {
            JsonRejection::JsonDataError(_) | JsonRejection::JsonSyntaxError(_) => {
                StatusCode::BAD_REQUEST
            }
            _ => e.status(),
        };
        ApiError::new(status, e.body_text())
    })
}

fn path_name(p: Result<Path<String>, PathRejection>) -> Result<String, ApiError> {
    p.map(|Path(name)| name)
        .map_err(|e| ApiError::new(e.status(), e.body_text()))
}

async fn create_fleet(
    State(state): State<AppState>,
    b: Result<Json<FleetRequest>, JsonRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let req = body(b)?;
    let name = fleet_name(&req.spec.name)?;
    Ok(Json(
        state
            .daemon
            .apply(&name, req.spec, req.credentials, false)
            .await?,
    ))
}

async fn update_fleet(
    State(state): State<AppState>,
    name: Result<Path<String>, PathRejection>,
    b: Result<Json<FleetRequest>, JsonRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let req = body(b)?;
    let name = fleet_name(&path_name(name)?)?;
    Ok(Json(
        state
            .daemon
            .apply(&name, req.spec, req.credentials, true)
            .await?,
    ))
}

async fn get_fleet(
    State(state): State<AppState>,
    name: Result<Path<String>, PathRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let name = fleet_name(&path_name(name)?)?;
    state
        .daemon
        .get(&name)
        .await
        .map(Json)
        .ok_or_else(|| DaemonError::NotFound.into())
}

async fn list_fleets(State(state): State<AppState>) -> Json<Vec<FleetSummary>> {
    Json(state.daemon.list().await)
}

async fn delete_fleet(
    State(state): State<AppState>,
    name: Result<Path<String>, PathRejection>,
    q: Result<Query<DownQuery>, QueryRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let Query(q) = q.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    if q.purge && (q.keep_repos || q.keep_sessions) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "purge cannot be combined with keep flags",
        ));
    }
    let name = fleet_name(&path_name(name)?)?;
    let keep = Keep {
        repos: q.keep_repos,
        sessions: q.keep_sessions,
    };
    Ok(Json(state.daemon.down(&name, keep, q.purge).await?))
}

/// `POST /v1/plugin-host/hello`: the plugin's bearer token, verified
/// against the `hecaton/plugins/<name>` secret; unknown plugin and bad
/// token answer alike.
async fn plugin_hello(
    State(state): State<AppState>,
    headers: HeaderMap,
    b: Result<Json<HelloRequest>, JsonRejection>,
) -> Result<Json<HelloResponse>, ApiError> {
    let unauthorized = || ApiError::new(StatusCode::UNAUTHORIZED, "unknown plugin or bad token");
    let Some(token) = bearer(&headers) else {
        return Err(unauthorized());
    };
    let req = b
        .map(|Json(r)| r)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    let name: AgentName = req.name.parse().map_err(|_: NameError| unauthorized())?;
    state
        .daemon
        .plugin_hello(&name, token, req)
        .await
        .map(Json)
        .map_err(|e| match e {
            DaemonError::Unauthorized => unauthorized(),
            other => other.into(),
        })
}

async fn list_plugins(State(state): State<AppState>) -> Json<Vec<PluginStatus>> {
    Json(state.daemon.plugins().list().await)
}

async fn sync_plugins(State(state): State<AppState>) -> Result<Json<SyncReport>, ApiError> {
    Ok(Json(state.daemon.sync_plugins().await?))
}

async fn purge_plugin(
    State(state): State<AppState>,
    name: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let name: AgentName = path_name(name)?
        .parse()
        .map_err(|e: NameError| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
    state.daemon.plugins().purge(&name).await?;
    Ok(Json(json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A KV failure must not render the daemon's on-disk path into a body
    /// a sandboxed plugin reads.
    #[test]
    fn a_kv_storage_error_is_a_fixed_500() {
        let e = ApiError::from(PluginError::Kv {
            path: "/home/op/.local/share/hecaton/plugins/flow/kv/k".into(),
            message: "permission denied".into(),
        });
        assert_eq!(
            (e.status, e.message.as_str()),
            (StatusCode::INTERNAL_SERVER_ERROR, "kv: storage error")
        );
    }
}
