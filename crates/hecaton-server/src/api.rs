//! The HTTP surface (Phase 3 spec §3.4): plain HTTP on loopback, admin
//! bearer on `/v1/fleets*`, per-agent secret on the events route.

use std::future::Future;
use std::sync::Arc;

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{StatusCode, header::CONTENT_TYPE};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hecaton_api::{DownQuery, ErrorBody, FleetRequest, FleetSummary};
use hecaton_core::{FleetName, FleetRecord, Keep};

use crate::auth::{RateLimiter, bearer, constant_time_eq};
use crate::daemon::{Daemon, DaemonError};
use crate::hooks;

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
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin))
        .layer(DefaultBodyLimit::max(4 << 20));
    let agents = Router::new()
        .route(
            "/v1/agents/{fleet}/{crew}/{agent}/events",
            post(hooks::events),
        )
        .layer(DefaultBodyLimit::max(1 << 20));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .merge(admin)
        .merge(agents)
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

async fn metrics(State(state): State<AppState>) -> Response {
    let snapshots = state.daemon.snapshots().await;
    state.daemon.metrics().set_gauges(&snapshots);
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.daemon.metrics().encode(),
    )
        .into_response()
}

fn fleet_name(s: &str) -> Result<FleetName, ApiError> {
    s.parse()
        .map_err(|e: hecaton_core::NameError| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))
}

fn body(b: Result<Json<FleetRequest>, JsonRejection>) -> Result<FleetRequest, ApiError> {
    b.map(|Json(r)| r)
        .map_err(|e| ApiError::new(e.status(), e.body_text()))
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
