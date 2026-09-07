//! `/v1/plugin-host/*` beyond `hello` (plugins spec §4.1): the plugin's
//! bearer identifies it, the manifest's `needs` gates every route.

use axum::body::Bytes;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use hecaton_api::{Capability, KvKeys, PluginAction};
use hecaton_core::{AgentId, AgentName, FleetName, FleetRecord, is_reserved_fleet};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{ApiError, AppState};
use crate::auth::bearer;
use crate::daemon::DaemonError;
use crate::plugins::PluginError;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/plugin-host/fleets", get(list_fleets))
        .route("/v1/plugin-host/fleets/{name}", get(get_fleet))
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/actions",
            axum::routing::post(post_action),
        )
        .route("/v1/plugin-host/kv", get(list_keys))
        .route(
            "/v1/plugin-host/kv/{*key}",
            get(get_key).put(put_key).delete(delete_key),
        )
}

/// The calling plugin, from its token, with the capability check.
async fn caller(
    state: &AppState,
    headers: &HeaderMap,
    cap: Capability,
) -> Result<AgentName, ApiError> {
    let unauthorized = || ApiError::new(StatusCode::UNAUTHORIZED, "unknown plugin or bad token");
    let token = bearer(headers).ok_or_else(unauthorized)?;
    let name = state
        .daemon
        .plugin_for_token(token)
        .await
        .ok_or_else(unauthorized)?;
    if !state.daemon.registry().has(&name, cap) {
        return Err(PluginError::Capability(crate::plugins::wire_label(cap)).into());
    }
    Ok(name)
}

async fn list_fleets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<FleetRecord>>, ApiError> {
    caller(&state, &headers, Capability::Fleets).await?;
    Ok(Json(state.daemon.plugin_fleets().await))
}

async fn get_fleet(
    State(state): State<AppState>,
    headers: HeaderMap,
    name: Result<Path<String>, PathRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    caller(&state, &headers, Capability::Fleets).await?;
    let Path(name) = name.map_err(|e| ApiError::new(e.status(), e.body_text()))?;
    let name: FleetName = name
        .parse()
        .map_err(|_: hecaton_core::NameError| ApiError::from(DaemonError::NotFound))?;
    if is_reserved_fleet(name.as_str()) {
        return Err(DaemonError::NotFound.into());
    }
    state
        .daemon
        .get(&name)
        .await
        .map(Json)
        .ok_or_else(|| DaemonError::NotFound.into())
}

async fn post_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
    body: Result<Json<PluginAction>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let plugin = caller(&state, &headers, Capability::Actions).await?;
    let Path((f, c, a)) = path.map_err(|e| ApiError::new(e.status(), e.body_text()))?;
    let agent: AgentId = format!("{f}/{c}/{a}")
        .parse()
        .map_err(|_: hecaton_core::NameError| ApiError::from(DaemonError::NotFound))?;
    let Json(action) = body.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    if !state.daemon.registry().is_active(&agent, &plugin) {
        return Err(PluginError::NotActive(agent.to_string()).into());
    }
    state
        .daemon
        .execute_action(&agent, &action, Some(plugin.as_str()))
        .await?;
    Ok(Json(json!({})))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct KvQuery {
    prefix: String,
    secret: bool,
}

fn kv_query(q: Result<Query<KvQuery>, QueryRejection>) -> Result<KvQuery, ApiError> {
    q.map(|Query(q)| q)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))
}

fn key_of(p: Result<Path<String>, PathRejection>) -> Result<String, ApiError> {
    p.map(|Path(k)| k)
        .map_err(|e| ApiError::new(e.status(), e.body_text()))
}

async fn list_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
    q: Result<Query<KvQuery>, QueryRejection>,
) -> Result<Json<KvKeys>, ApiError> {
    let plugin = caller(&state, &headers, Capability::Kv).await?;
    let q = kv_query(q)?;
    let kv = state.daemon.kv().clone();
    let keys = tokio::task::spawn_blocking(move || kv.list(&plugin, &q.prefix))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(KvKeys { keys }))
}

async fn get_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let plugin = caller(&state, &headers, Capability::Kv).await?;
    let key = key_of(key)?;
    let kv = state.daemon.kv().clone();
    let value = tokio::task::spawn_blocking(move || kv.get(&plugin, &key))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    match value {
        Some(bytes) => Ok(([(CONTENT_TYPE, "application/octet-stream")], bytes).into_response()),
        None => Err(ApiError::new(StatusCode::NOT_FOUND, "no such key")),
    }
}

async fn put_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Result<Path<String>, PathRejection>,
    q: Result<Query<KvQuery>, QueryRejection>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let plugin = caller(&state, &headers, Capability::Kv).await?;
    let key = key_of(key)?;
    let q = kv_query(q)?;
    let kv = state.daemon.kv().clone();
    tokio::task::spawn_blocking(move || kv.put(&plugin, &key, &body, q.secret))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(json!({})))
}

async fn delete_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let plugin = caller(&state, &headers, Capability::Kv).await?;
    let key = key_of(key)?;
    let kv = state.daemon.kv().clone();
    tokio::task::spawn_blocking(move || kv.delete(&plugin, &key))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(json!({})))
}

/// The metrics prefix rule (plugins spec §9): every family name in the
/// body starts with `hecaton_plugin_<name>_`. Comment lines are checked
/// through their `# TYPE`/`# HELP` family, sample lines through the name
/// before `{` or the first space.
pub fn families_ok(body: &str, plugin: &str) -> bool {
    let prefix = format!("hecaton_plugin_{plugin}_");
    body.lines().all(|line| {
        let line = line.trim();
        if line.is_empty() {
            return true;
        }
        let family = if let Some(rest) = line.strip_prefix('#') {
            let mut words = rest.split_whitespace();
            match words.next() {
                Some("TYPE" | "HELP") => match words.next() {
                    Some(f) => f,
                    None => return false,
                },
                _ => return true,
            }
        } else {
            line.split(|c: char| c == '{' || c.is_whitespace())
                .next()
                .unwrap_or("")
        };
        family.starts_with(&prefix)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_passes_only_if_every_family_carries_the_plugin_prefix() {
        let ok = "# HELP hecaton_plugin_flow_state Current state\n# TYPE hecaton_plugin_flow_state gauge\nhecaton_plugin_flow_state{agent=\"a\"} 1\n\nhecaton_plugin_flow_transitions_total{from=\"a\",to=\"b\"} 2\n";
        assert!(families_ok(ok, "flow"));
        assert!(!families_ok(ok, "web"), "another plugin's prefix");
        assert!(!families_ok(
            "hecaton_plugin_flow_x 1\nprocess_cpu_seconds_total 3\n",
            "flow"
        ));
        assert!(
            !families_ok("hecaton_agents{fleet=\"f\"} 1\n", "flow"),
            "daemon families cannot be spoofed"
        );
        assert!(families_ok("", "flow"), "empty is fine");
        assert!(families_ok("# just a comment\n", "flow"));
        assert!(
            !families_ok("hecaton_plugin_flow 1\n", "flow"),
            "the prefix needs the trailing underscore"
        );
    }
}
