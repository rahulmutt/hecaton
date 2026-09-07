//! The pages and the bridge (plugins spec §18.5), under the daemon's
//! mount: every link is built from `X-Hecaton-Forwarded-Prefix`. The
//! index polls `agents.json`; the terminal page runs the vendored
//! xterm.js against `agents/{id}/ws`, which relays to the daemon's attach.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use hecaton_api::ResizeFrame;

use crate::plugin::Shared;
use crate::state::AgentRow;

pub const PREFIX_HEADER: &str = "x-hecaton-forwarded-prefix";

const XTERM_JS: &[u8] = include_bytes!("../assets/xterm.js");
const XTERM_CSS: &[u8] = include_bytes!("../assets/xterm.css");
const ADDON_FIT_JS: &[u8] = include_bytes!("../assets/addon-fit.js");
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// How often the index re-fetches `agents.json`, in milliseconds.
const INDEX_POLL_MS: u32 = 2000;

pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/agents.json", get(agents_json))
        .route("/agents/{fleet}/{crew}/{agent}", get(terminal))
        .route("/agents/{fleet}/{crew}/{agent}/ws", get(bridge_route))
        .route("/assets/{file}", get(asset))
        .with_state(shared)
}

/// The mount the daemon put us under, without a trailing slash; empty
/// when called directly (tests, curl).
fn prefix(headers: &HeaderMap) -> String {
    headers
        .get(PREFIX_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_default()
}

pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn phase_label(row: &AgentRow) -> String {
    serde_json::to_value(row.phase)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The index: rows rendered server-side, then refreshed by a small script
/// that rebuilds them from `agents.json` with `textContent` — nothing a
/// message says can become markup.
pub fn index_html(prefix: &str, rows: &[AgentRow]) -> String {
    let mut body = String::new();
    for r in rows {
        body.push_str(&format!(
            "<tr><td><a href=\"{p}/agents/{id}\">{id}</a></td><td>{phase}</td><td>{msg}</td></tr>\n",
            p = html_escape(prefix),
            id = html_escape(&r.id),
            phase = html_escape(&phase_label(r)),
            msg = html_escape(&r.message),
        ));
    }
    format!(
        r##"<!doctype html>
<html><head><meta charset="utf-8"><title>hecaton</title>
<style>body{{font:14px system-ui,sans-serif;margin:2rem}}table{{border-collapse:collapse}}td,th{{padding:.3rem .8rem;text-align:left;border-bottom:1px solid #ddd}}</style>
</head><body>
<h1>hecaton agents</h1>
<table><thead><tr><th>agent</th><th>phase</th><th>message</th></tr></thead>
<tbody id="rows">
{body}</tbody></table>
<p id="empty" hidden>no agents enabled for web</p>
<script>
const prefix = "{p}";
async function refresh() {{
  try {{
    const rows = await (await fetch("{p}/agents.json")).json();
    const tbody = document.getElementById("rows");
    tbody.replaceChildren();
    for (const r of rows) {{
      const tr = document.createElement("tr");
      const a = document.createElement("a");
      a.href = prefix + "/agents/" + r.id;
      a.textContent = r.id;
      const c1 = document.createElement("td"); c1.appendChild(a);
      const c2 = document.createElement("td"); c2.textContent = r.phase;
      const c3 = document.createElement("td"); c3.textContent = r.message;
      tr.append(c1, c2, c3);
      tbody.appendChild(tr);
    }}
    document.getElementById("empty").hidden = rows.length > 0;
  }} catch (e) {{ console.warn("refresh failed", e); }}
}}
document.getElementById("empty").hidden = document.querySelectorAll("#rows tr").length > 0;
setInterval(refresh, {poll});
</script>
</body></html>
"##,
        p = html_escape(prefix),
        poll = INDEX_POLL_MS,
    )
}

/// The terminal page: xterm.js on a full-window div, the bridge socket,
/// resizes on fit and on window resize, a line when the socket closes.
/// A resize is only sent with both dimensions non-zero: the fit addon
/// reports zeroes for a hidden container and the daemon closes the attach
/// with 1003 on a zero-sized frame (plugins spec §18.4).
pub fn terminal_html(prefix: &str, id: &str) -> String {
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>{id}</title>
<link rel="stylesheet" href="{p}/assets/xterm.css">
<style>html,body{{height:100%;margin:0;background:#000}}#t{{height:100%}}</style>
</head><body><div id="t"></div>
<script src="{p}/assets/xterm.js"></script>
<script src="{p}/assets/addon-fit.js"></script>
<script>
const term = new Terminal({{ cursorBlink: true, fontSize: 14 }});
const fit = new FitAddon.FitAddon();
term.loadAddon(fit);
term.open(document.getElementById("t"));
fit.fit();
const scheme = location.protocol === "https:" ? "wss://" : "ws://";
const ws = new WebSocket(scheme + location.host + "{p}/agents/{id}/ws");
ws.binaryType = "arraybuffer";
const resize = () => {{
  if (ws.readyState === WebSocket.OPEN && term.cols > 0 && term.rows > 0) {{
    ws.send(JSON.stringify({{ resize: {{ cols: term.cols, rows: term.rows }} }}));
  }}
}};
ws.onopen = resize;
ws.onmessage = (e) => term.write(new Uint8Array(e.data));
ws.onclose = (e) => term.write("\r\n[disconnected" + (e.reason ? ": " + e.reason : "") + "]\r\n");
const enc = new TextEncoder();
term.onData((d) => {{ if (ws.readyState === WebSocket.OPEN) ws.send(enc.encode(d)); }});
window.addEventListener("resize", () => {{ fit.fit(); resize(); }});
term.focus();
</script>
</body></html>
"#,
        p = html_escape(prefix),
        id = html_escape(id),
    )
}

async fn index(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Html<String> {
    Html(index_html(&prefix(&headers), &shared.cache.rows()))
}

async fn agents_json(State(shared): State<Arc<Shared>>) -> Json<Vec<AgentRow>> {
    Json(shared.cache.rows())
}

async fn terminal(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Path((fleet, crew, agent)): Path<(String, String, String)>,
) -> Response {
    let id = format!("{fleet}/{crew}/{agent}");
    if !shared.cache.is_enabled(&id) {
        return (StatusCode::NOT_FOUND, "no such agent").into_response();
    }
    Html(terminal_html(&prefix(&headers), &id)).into_response()
}

async fn bridge_route(
    State(shared): State<Arc<Shared>>,
    Path((fleet, crew, agent)): Path<(String, String, String)>,
    ws: WebSocketUpgrade,
) -> Response {
    let id = format!("{fleet}/{crew}/{agent}");
    if !shared.cache.is_enabled(&id) {
        return (StatusCode::NOT_FOUND, "no such agent").into_response();
    }
    ws.on_upgrade(move |socket| bridge(socket, shared, id))
}

async fn asset(Path(file): Path<String>) -> Response {
    let (kind, bytes) = match file.as_str() {
        "xterm.js" => ("text/javascript; charset=utf-8", XTERM_JS),
        "xterm.css" => ("text/css; charset=utf-8", XTERM_CSS),
        "addon-fit.js" => ("text/javascript; charset=utf-8", ADDON_FIT_JS),
        _ => return (StatusCode::NOT_FOUND, "no such asset").into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, IMMUTABLE),
        ],
        Bytes::from_static(bytes),
    )
        .into_response()
}

async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_string().into(),
        })))
        .await;
}

/// One browser tab ↔ one daemon attach: binary both ways, the browser's
/// resize text frames forwarded as resizes, everything else ignored.
/// Ends when either side closes.
pub async fn bridge(mut browser: WebSocket, shared: Arc<Shared>, agent: String) {
    let attach = match shared.host.attach(&agent).await {
        Ok(a) => a,
        Err(e) => {
            close(&mut browser, 1011, &format!("attach: {e}")).await;
            return;
        }
    };
    let (mut rd, mut wr) = attach.split();
    shared.terminals_open.inc();
    shared.terminals_total.inc();
    loop {
        tokio::select! {
            frame = rd.read() => match frame {
                Some(bytes) => {
                    if browser.send(Message::Binary(Bytes::from(bytes))).await.is_err() {
                        break;
                    }
                }
                None => {
                    close(&mut browser, 1000, "the terminal closed").await;
                    break;
                }
            },
            msg = browser.recv() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    if wr.write(&bytes).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if let Some(f) = ResizeFrame::parse(text.as_str())
                        && wr.resize(f.resize.cols, f.resize.rows).await.is_err()
                    {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    wr.close().await;
    shared.terminals_open.dec();
}

#[cfg(test)]
mod tests {
    use super::*;
    use hecaton_api::AgentPhase;

    #[test]
    fn pages_escape_what_they_render_and_link_through_the_prefix() {
        assert_eq!(
            html_escape("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
        let rows = vec![AgentRow {
            id: "f/c/a".into(),
            phase: AgentPhase::Dead,
            message: "<script>alert(1)</script>".into(),
        }];
        let html = index_html("/v1/plugins/web", &rows);
        assert!(html.contains(r#"<a href="/v1/plugins/web/agents/f/c/a">f/c/a</a>"#));
        assert!(html.contains("<td>dead</td>"));
        assert!(html.contains("&lt;script&gt;") && !html.contains("<script>alert"));
        assert!(html.contains(r#"fetch("/v1/plugins/web/agents.json")"#));
        let html = index_html("", &[]);
        assert!(
            html.contains(r#"fetch("/agents.json")"#),
            "no prefix: relative to the root"
        );
        let page = terminal_html("/v1/plugins/web", "f/c/a");
        assert!(page.contains(r#"src="/v1/plugins/web/assets/xterm.js""#));
        assert!(page.contains(r#""/v1/plugins/web/agents/f/c/a/ws""#));
        let page = terminal_html("/p", "<x>");
        assert!(page.contains("&lt;x&gt;") && !page.contains("<x>"));
    }
}
