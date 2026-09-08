//! The pages and the bridge (plugins spec §18.5), under the daemon's
//! mount: every link is built from `X-Hecaton-Forwarded-Prefix`. The
//! index polls `agents.json`; the terminal page runs the vendored
//! xterm.js against `agents/{id}/ws`, which relays to the daemon's attach.

use std::sync::{Arc, LazyLock};

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use hecaton_api::{ResizeFrame, TextFrame};
use hecaton_plugin_sdk::metrics::IntGauge;
use sha2::{Digest, Sha256};

use crate::plugin::Shared;
use crate::state::AgentRow;

pub const PREFIX_HEADER: &str = "x-hecaton-forwarded-prefix";

const XTERM_JS: &[u8] = include_bytes!("../assets/xterm.js");
const XTERM_CSS: &[u8] = include_bytes!("../assets/xterm.css");
const ADDON_FIT_JS: &[u8] = include_bytes!("../assets/addon-fit.js");
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// The first twelve hex digits of the sha256 over the three vendored
/// files, in every asset path: a vendor bump is a new URL, so `IMMUTABLE`
/// can never hand a browser that visited before a stale bundle.
pub static ASSET_DIGEST: LazyLock<String> = LazyLock::new(|| {
    let mut h = Sha256::new();
    h.update(XTERM_JS);
    h.update(XTERM_CSS);
    h.update(ADDON_FIT_JS);
    hex::encode(h.finalize())[..12].to_string()
});
/// How often the index re-fetches `agents.json`, in milliseconds.
const INDEX_POLL_MS: u32 = 2000;

pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/agents.json", get(agents_json))
        .route("/agents/{fleet}/{crew}/{agent}", get(terminal))
        .route("/agents/{fleet}/{crew}/{agent}/ws", get(bridge_route))
        .route(
            "/agents/{fleet}/{crew}/{agent}/events.json",
            get(events_json),
        )
        .route("/assets/{digest}/{file}", get(asset))
        .with_state(shared)
}

/// The mount the daemon put us under, without a trailing slash; empty
/// when called directly (tests, curl). The proxy overwrites the header
/// and direct access needs the plugin's bearer, so a value that is not
/// a path could only come from a caller who already holds the token —
/// it is still not put into a page.
fn prefix(headers: &HeaderMap) -> String {
    headers
        .get(PREFIX_HEADER)
        .and_then(|v| v.to_str().ok())
        // one leading slash, not two (or a backslash): `//host` is a
        // scheme-relative URL to a browser, and it would land in a `src`
        .filter(|s| s.starts_with('/') && !s[1..].starts_with(['/', '\\']))
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
/// reports zeroes for a hidden container, and the daemon ignores such a
/// frame anyway (plugins spec §18.8), so it is not worth sending.
pub fn terminal_html(prefix: &str, id: &str) -> String {
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>{id}</title>
<link rel="stylesheet" href="{p}/assets/{d}/xterm.css">
<style>html,body{{height:100%;margin:0;background:#000}}#t{{height:100%}}</style>
</head><body><div id="t"></div>
<script src="{p}/assets/{d}/xterm.js"></script>
<script src="{p}/assets/{d}/addon-fit.js"></script>
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
        d = ASSET_DIGEST.as_str(),
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

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct AfterQuery {
    after: u64,
}

/// The activity column's poll: entries after `after`, and the phase.
async fn events_json(
    State(shared): State<Arc<Shared>>,
    Path((fleet, crew, agent)): Path<(String, String, String)>,
    q: Result<Query<AfterQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let id = format!("{fleet}/{crew}/{agent}");
    if !shared.cache.is_enabled(&id) {
        return (StatusCode::NOT_FOUND, "no such agent").into_response();
    }
    let after = match q {
        Ok(Query(q)) => q.after,
        Err(e) => return (StatusCode::BAD_REQUEST, e.body_text()).into_response(),
    };
    Json(shared.cache.events_after(&id, after)).into_response()
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

async fn asset(Path((digest, file)): Path<(String, String)>) -> Response {
    if digest != *ASSET_DIGEST {
        return (StatusCode::NOT_FOUND, "no such asset bundle").into_response();
    }
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

/// A close frame's reason fits in the control frame: 123 bytes at most,
/// cut at a character boundary (a longer one makes the browser fail the
/// connection with 1002 and show nothing).
fn close_reason(reason: &str) -> String {
    let mut end = reason.len().min(123);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_string()
}

async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: close_reason(reason).into(),
        })))
        .await;
}

/// Holds `terminals_open` up for as long as it lives, so a bridge future
/// dropped mid-way (server shutdown, an aborted task) still lets the
/// gauge fall.
struct OpenTerminal(IntGauge);

impl OpenTerminal {
    fn new(gauge: &IntGauge) -> Self {
        gauge.inc();
        Self(gauge.clone())
    }
}

impl Drop for OpenTerminal {
    fn drop(&mut self) {
        self.0.dec();
    }
}

/// A close code that may be sent in a close frame: the daemon's own
/// (1000, 1003, 1011) cross as they are; 1005, 1006 and 1015 are reserved
/// for a socket that ended without one and become 1011.
fn forwardable(code: u16) -> u16 {
    match code {
        1004..=1006 | 1015 => 1011,
        1000..=1013 | 3000..=4999 => code,
        _ => 1011,
    }
}

/// One browser tab ↔ one daemon attach: binary both ways, the browser's
/// resize text frames forwarded as resizes, everything else ignored.
/// Ends when either side closes; the daemon's close code and reason
/// reach the browser, so the page can say why.
pub async fn bridge(mut browser: WebSocket, shared: Arc<Shared>, agent: String) {
    let attach = match shared.host.attach(&agent).await {
        Ok(a) => a,
        Err(e) => {
            close(&mut browser, 1011, &format!("attach: {e}")).await;
            return;
        }
    };
    let (mut rd, mut wr) = attach.split();
    let _open = OpenTerminal::new(&shared.terminals_open);
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
                    let (code, reason) = match rd.close_reason() {
                        Some(why) if !why.reason.is_empty() => (forwardable(why.code), why.reason.clone()),
                        Some(why) => (forwardable(why.code), "the terminal closed".to_string()),
                        None => (1000, "the terminal closed".to_string()),
                    };
                    close(&mut browser, code, &reason).await;
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
                    if let TextFrame::Resize(f) = ResizeFrame::parse(text.as_str())
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
        let d = ASSET_DIGEST.as_str();
        assert_eq!(d.len(), 12);
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()), "{d}");
        assert!(page.contains(&format!(r#"src="/v1/plugins/web/assets/{d}/xterm.js""#)));
        assert!(page.contains(r#""/v1/plugins/web/agents/f/c/a/ws""#));
        let page = terminal_html("/p", "<x>");
        assert!(page.contains("&lt;x&gt;") && !page.contains("<x>"));
    }

    /// The proxy overwrites the header, and direct access needs the
    /// plugin's bearer; the check is defence in depth at the boundary.
    #[test]
    fn a_forwarded_prefix_is_a_path_or_nothing() {
        let with = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(PREFIX_HEADER, v.parse().unwrap());
            prefix(&h)
        };
        assert_eq!(with("/v1/plugins/web"), "/v1/plugins/web");
        assert_eq!(with("/v1/plugins/web/"), "/v1/plugins/web");
        assert_eq!(with("http://evil.example"), "", "not a path");
        assert_eq!(with("//evil.example"), "", "scheme-relative, not a path");
        assert_eq!(with("/\\evil.example"), "");
        assert_eq!(with("javascript:alert(1)"), "");
        assert_eq!(with(""), "");
        assert_eq!(prefix(&HeaderMap::new()), "");
    }

    #[test]
    fn the_open_terminals_gauge_falls_with_the_guard() {
        let gauge = hecaton_plugin_sdk::metrics::IntGauge::new("open", "open").unwrap();
        let a = OpenTerminal::new(&gauge);
        let b = OpenTerminal::new(&gauge);
        assert_eq!(gauge.get(), 2);
        drop(a);
        assert_eq!(gauge.get(), 1);
        drop(b);
        assert_eq!(gauge.get(), 0);
    }

    #[test]
    fn a_close_reason_fits_the_control_frame() {
        assert_eq!(close_reason("short"), "short");
        let long = "é".repeat(100); // 200 bytes
        let cut = close_reason(&long);
        assert!(cut.len() <= 123 && cut.chars().all(|c| c == 'é'), "{cut:?}");
        assert_eq!(cut.len(), 122, "cut at a character boundary");
    }

    #[test]
    fn only_sendable_close_codes_are_forwarded() {
        assert_eq!(forwardable(1000), 1000);
        assert_eq!(forwardable(1003), 1003);
        assert_eq!(forwardable(1011), 1011);
        assert_eq!(forwardable(4000), 4000);
        // reserved: never on the wire
        assert_eq!(forwardable(1004), 1011);
        assert_eq!(forwardable(1005), 1011);
        assert_eq!(forwardable(1006), 1011);
        assert_eq!(forwardable(1015), 1011);
        assert_eq!(forwardable(2000), 1011);
    }
}
