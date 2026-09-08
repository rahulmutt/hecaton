//! Plugins spec §18.2 through a real listener: login codes, the session
//! cookie, the same-origin rule, and (Task 3) the proxied mount.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Router, body::Bytes};
use futures_util::{SinkExt, StreamExt};
use hecaton_core::plugin_id;
use hecaton_plugin_sdk::{Env, Host};
use serde_json::json;
use support::{World, world};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_login_code_becomes_a_cookie_once() {
    let w = world().await;
    let (s, v) = w.api.admin(
        "POST",
        "/v1/sessions",
        Some(&json!({ "to": "/v1/plugins/web/" })),
    );
    assert_eq!(s, 200, "{v}");
    let url = v["login_url"].as_str().unwrap().to_string();
    assert!(
        url.starts_with(&format!("{}/v1/login/", w.daemon.origin())),
        "{url}"
    );
    assert!(url.ends_with("?to=/v1/plugins/web/"), "{url}");
    let path = url.trim_start_matches(w.daemon.origin()).to_string();
    let (s, _) = w.api.call("POST", "/v1/sessions", None, Some(&json!({})));
    assert_eq!(s, 401, "admin only");
    let (s, v) = w
        .api
        .admin("POST", "/v1/sessions", Some(&json!({ "to": "/v1/fleets" })));
    assert_eq!(
        (s, v["error"].as_str().unwrap()),
        (400, "to: must be a path under /v1/plugins/")
    );

    let (s, headers, _) = w.api.raw("GET", &path, &[], None);
    assert_eq!(s, 303);
    assert_eq!(header(&headers, "location"), Some("/v1/plugins/web/"));
    let cookie = header(&headers, "set-cookie").unwrap().to_string();
    assert!(
        cookie.starts_with("hecaton_session=")
            && cookie.ends_with("; HttpOnly; SameSite=Strict; Path=/v1/plugins/"),
        "{cookie}"
    );
    let id = cookie
        .split(';')
        .next()
        .unwrap()
        .trim_start_matches("hecaton_session=");
    assert_eq!(id.len(), 64);
    assert!(w.daemon.sessions().is_valid(id));

    let (s, _, text) = w.api.raw("GET", &path, &[], None);
    assert_eq!(s, 404, "single use");
    assert!(
        text.starts_with("login code already used ") && text.ends_with("s ago"),
        "a second visit is told the code was spent: {text:?}"
    );
    let (s, _, text) = w.api.raw("GET", "/v1/login/nope", &[], None);
    assert_eq!((s, text.as_str()), (404, "unknown login code"));

    // The mount root without its slash is where a hand-typed or
    // truncated URL lands: a bare redirect to the mount, no auth needed,
    // while DELETE there is still the admin purge route.
    let (s, headers, _) = w.api.raw("GET", "/v1/plugins/web", &[], None);
    assert_eq!(s, 308, "slash-less mount root redirects");
    assert_eq!(header(&headers, "location"), Some("/v1/plugins/web/"));
    let (s, _, _) = w.api.raw("GET", "/v1/plugins/not%20a%20name", &[], None);
    assert_eq!(s, 404, "a name that cannot be a plugin is not redirected");
    let (s, v) = w.api.call("DELETE", "/v1/plugins/web", None, None);
    assert_eq!(
        (s, v["error"].as_str().unwrap()),
        (401, "missing or invalid admin token"),
        "purge still needs the bearer"
    );
    let (s, _, text) = w.api.raw("GET", "/v1/login/nope?to=/v1/fleets", &[], None);
    assert_eq!(
        (s, text.as_str()),
        (400, "to: must be a path under /v1/plugins/")
    );
}

async fn token(w: &World, plugin: &str) -> String {
    w.daemon
        .hook_secret(&plugin_id(&plugin.parse().unwrap()))
        .await
        .unwrap()
}

/// A plugin with routes, outside the SDK (the SDK's `routes` is Task 6):
/// its root echoes the prefix and the bearer it was given, `headers`
/// dumps what arrived, `post` answers the body length, `echo` is a
/// WebSocket echo. Every route demands `expect` as the bearer.
async fn routes_plugin(expect: String) -> String {
    let expect = Arc::new(expect);
    let check = {
        let expect = expect.clone();
        move |headers: &HeaderMap| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == format!("Bearer {expect}"))
        }
    };
    let c1 = check.clone();
    let c2 = check.clone();
    let c3 = check.clone();
    let app = Router::new()
        .route(
            "/v1/routes",
            get(move |headers: HeaderMap| async move {
                if !c1(&headers) {
                    return (axum::http::StatusCode::UNAUTHORIZED, String::new());
                }
                let prefix = headers
                    .get("x-hecaton-forwarded-prefix")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-");
                (axum::http::StatusCode::OK, format!("root prefix={prefix}"))
            }),
        )
        .route(
            "/v1/routes/headers",
            get(move |headers: HeaderMap| async move {
                let map: BTreeMap<String, String> = headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect();
                serde_json::to_string(&map).unwrap()
            }),
        )
        .route(
            "/v1/routes/post",
            post(move |headers: HeaderMap, body: Bytes| async move {
                if !c2(&headers) {
                    return (axum::http::StatusCode::UNAUTHORIZED, String::new());
                }
                (axum::http::StatusCode::OK, body.len().to_string())
            }),
        )
        .route(
            "/v1/routes/echo",
            get(move |headers: HeaderMap, ws: WebSocketUpgrade| async move {
                if !c3(&headers) {
                    return axum::http::StatusCode::UNAUTHORIZED.into_response();
                }
                ws.on_upgrade(|mut socket| async move {
                    while let Some(Ok(msg)) = socket.recv().await {
                        if let Message::Binary(b) = msg {
                            let _ = socket.send(Message::Binary(b)).await;
                        }
                    }
                })
            }),
        )
        // anything else echoes the raw path, so a test can see exactly what
        // the proxy put on the wire
        .fallback(
            |req: axum::extract::Request| async move { format!("path={}", req.uri().path()) },
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listen = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    listen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mount_proxies_plain_requests_and_websockets_and_filters_headers() {
    let w = world().await;
    let admin = [("Authorization", format!("Bearer {}", w.api.token()))];
    let admin: Vec<(&str, &str)> = admin.iter().map(|(k, v)| (*k, v.as_str())).collect();

    // before hello: 503; unknown or routeless: 404; no auth: 401
    let (s, _, text) = w.api.raw("GET", "/v1/plugins/web/", &admin, None);
    assert_eq!(
        (s, text.as_str()),
        (503, "{\"error\":\"plugin \\\"web\\\" is not ready\"}")
    );
    let (s, _, text) = w.api.raw("GET", "/v1/plugins/flow/", &admin, None);
    assert_eq!(
        (s, text.as_str()),
        (404, "{\"error\":\"plugin \\\"flow\\\" has no routes\"}")
    );
    let (s, _, _) = w.api.raw("GET", "/v1/plugins/nope/x", &admin, None);
    assert_eq!(s, 404);
    let (s, _, _) = w.api.raw("GET", "/v1/plugins/web/", &[], None);
    assert_eq!(s, 401);
    let (s, _, _) = w.api.raw(
        "GET",
        "/v1/plugins/web/",
        &[("Authorization", "Bearer wrong")],
        None,
    );
    assert_eq!(s, 401);

    let web = token(&w, "web").await;
    let listen = routes_plugin(web.clone()).await;
    Host::new(Env {
        api_url: w.api.base.clone(),
        name: "web".into(),
        token: web.clone(),
        scratch: w.dir.path().join("s"),
    })
    .unwrap()
    .hello("0.1.0", &listen)
    .await
    .unwrap();

    let (s, _, text) = w.api.raw("GET", "/v1/plugins/web/", &admin, None);
    assert_eq!(
        (s, text.as_str()),
        (200, "root prefix=/v1/plugins/web"),
        "the root maps to /v1/routes and the bearer is the plugin's own"
    );
    let mut with_junk = admin.clone();
    with_junk.push(("Cookie", "hecaton_session=stolen"));
    with_junk.push(("X-Custom", "1"));
    with_junk.push(("Connection", "keep-alive"));
    let (s, _, text) = w
        .api
        .raw("GET", "/v1/plugins/web/headers?q=1", &with_junk, None);
    assert_eq!(s, 200);
    let seen: BTreeMap<String, String> = serde_json::from_str(&text).unwrap();
    assert_eq!(seen.get("x-custom").map(String::as_str), Some("1"));
    assert_eq!(
        seen.get("x-hecaton-forwarded-prefix").map(String::as_str),
        Some("/v1/plugins/web")
    );
    assert_eq!(
        seen.get("authorization").map(String::as_str),
        Some(format!("Bearer {web}").as_str()),
        "the plugin's token, not the admin's"
    );
    assert!(!seen.contains_key("cookie"), "{seen:?}");
    assert!(!seen.contains_key("connection"), "{seen:?}");
    assert_eq!(
        seen.get("host").map(|h| h == &listen),
        Some(true),
        "host is the upstream's"
    );

    let (s, _, text) = w.api.raw(
        "POST",
        "/v1/plugins/web/post",
        &admin,
        Some(&[b'x'; 100][..]),
    );
    assert_eq!((s, text.as_str()), (200, "100"));
    let big = vec![b'x'; (1 << 20) + 1];
    let (s, _, _) = w
        .api
        .raw("POST", "/v1/plugins/web/post", &admin, Some(&big[..]));
    assert_eq!(s, 413);

    // the rest of the path crosses as the client encoded it, and a dot
    // segment is refused rather than resolved by the plugin
    let (s, _, text) = w
        .api
        .raw("GET", "/v1/plugins/web/a%20b%3Fx=1%2Fy", &admin, None);
    assert_eq!(
        (s, text.as_str()),
        (200, "path=/v1/routes/a%20b%3Fx=1%2Fy"),
        "axum's Path would have decoded this into a query and a separator"
    );
    for bad in [
        "/v1/plugins/web/../hook",
        "/v1/plugins/web/%2e%2e/hook",
        "/v1/plugins/web/a/%2E%2E/b",
        "/v1/plugins/web/./a",
    ] {
        let (s, _, text) = w.api.raw("GET", bad, &admin, None);
        assert_eq!(
            (s, text.as_str()),
            (
                400,
                "{\"error\":\"path: . and .. segments are not forwarded\"}"
            ),
            "{bad}"
        );
    }

    // a WebSocket through the mount: upgraded on both sides, bytes copied
    let url = format!(
        "{}/v1/plugins/web/echo",
        w.api.base.replacen("http://", "ws://", 1)
    );
    let mut req = url.clone().into_client_request().unwrap();
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {}", w.api.token()).parse().unwrap(),
    );
    let (mut ws, _) = connect_async(req)
        .await
        .expect("spec §11.1: the upgrade passthrough on hyper-util's legacy client");
    ws.send(tokio_tungstenite::tungstenite::Message::Binary(
        b"ping".to_vec().into(),
    ))
    .await
    .unwrap();
    let echo = ws.next().await.unwrap().unwrap();
    assert_eq!(echo.into_data().as_ref(), b"ping");
    ws.close(None).await.unwrap();
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer wrong".parse().unwrap());
    let e = connect_async(req).await.unwrap_err();
    assert!(
        matches!(e, tokio_tungstenite::tungstenite::Error::Http(ref r) if r.status() == 401),
        "{e}"
    );

    // a session cookie works like the bearer, from the daemon's origin only
    let (_, v) = w.api.admin(
        "POST",
        "/v1/sessions",
        Some(&json!({ "to": "/v1/plugins/web/" })),
    );
    let path = v["login_url"]
        .as_str()
        .unwrap()
        .trim_start_matches(w.daemon.origin())
        .to_string();
    let (_, headers, _) = w.api.raw("GET", &path, &[], None);
    let cookie = header(&headers, "set-cookie")
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let (s, _, text) = w.api.raw(
        "GET",
        "/v1/plugins/web/",
        &[("Cookie", &cookie), ("Sec-Fetch-Site", "same-origin")],
        None,
    );
    assert_eq!((s, text.as_str()), (200, "root prefix=/v1/plugins/web"));
    let (s, _, _) = w
        .api
        .raw("GET", "/v1/plugins/web/", &[("Cookie", &cookie)], None);
    assert_eq!(s, 200, "a navigation sends no origin");
    let origin = w.daemon.origin().to_string();
    let (s, _, _) = w.api.raw(
        "GET",
        "/v1/plugins/web/",
        &[("Cookie", &cookie), ("Origin", &origin)],
        None,
    );
    assert_eq!(s, 200);
    let (s, _, text) = w.api.raw(
        "GET",
        "/v1/plugins/web/",
        &[("Cookie", &cookie), ("Origin", "http://evil.example")],
        None,
    );
    assert_eq!(
        (s, text.as_str()),
        (403, "{\"error\":\"cross-origin request refused\"}")
    );
    let (s, _, _) = w.api.raw(
        "GET",
        "/v1/plugins/web/",
        &[("Cookie", &cookie), ("Sec-Fetch-Site", "cross-site")],
        None,
    );
    assert_eq!(s, 403);
    let (s, _, _) = w.api.raw(
        "GET",
        "/v1/plugins/web/",
        &[("Cookie", "hecaton_session=forged")],
        None,
    );
    assert_eq!(s, 401);

    // Through a TLS-terminating reverse proxy, a browser's WebSocket
    // handshake carries the proxy's hostname as both Host and Origin and
    // no Sec-Fetch-Site; that is the page's own host, so it passes. A
    // foreign Origin against the same Host is a hijack attempt.
    let ws_url = format!(
        "{}/v1/plugins/web/echo",
        w.api.base.replacen("http://", "ws://", 1)
    );
    let proxied = |origin: &str| {
        let mut req = ws_url.clone().into_client_request().unwrap();
        let h = req.headers_mut();
        h.insert("host", "hecaton-7643.example.test".parse().unwrap());
        h.insert("origin", origin.parse().unwrap());
        h.insert("cookie", cookie.parse().unwrap());
        req
    };
    let (mut ws, _) = connect_async(proxied("https://hecaton-7643.example.test"))
        .await
        .expect("a proxied handshake whose Origin is its Host");
    ws.send(tokio_tungstenite::tungstenite::Message::Binary(
        b"ping".to_vec().into(),
    ))
    .await
    .unwrap();
    assert_eq!(
        ws.next().await.unwrap().unwrap().into_data().as_ref(),
        b"ping"
    );
    ws.close(None).await.unwrap();
    let e = connect_async(proxied("https://evil.example"))
        .await
        .unwrap_err();
    assert!(
        matches!(e, tokio_tungstenite::tungstenite::Error::Http(ref r) if r.status() == 403),
        "{e}"
    );
    let (s, _, _) = w.api.raw("GET", "/v1/fleets", &[("Cookie", &cookie)], None);
    assert_eq!(s, 401, "the cookie opens the mount and nothing else");

    // an unauthenticated guess mints no label of its own
    let (s, _, _) = w.api.raw("GET", "/v1/plugins/guessed-name/", &[], None);
    assert_eq!(s, 401);

    let (_, m) = w.api.call("GET", "/metrics", None, None);
    let m = m.as_str().unwrap();
    assert!(
        m.contains("hecaton_plugin_proxy_requests_total{plugin=\"web\",status=\"200\"}"),
        "{m}"
    );
    assert!(
        m.contains("hecaton_plugin_proxy_requests_total{plugin=\"web\",status=\"413\"} 1"),
        "{m}"
    );
    assert!(
        m.contains("hecaton_plugin_proxy_requests_total{plugin=\"web\",status=\"400\"} 4"),
        "{m}"
    );
    // every name the caller made up, every routeless plugin and every
    // request that never authenticated land on the one `unknown` series
    // (`flow` has no routes, `nope` and `guessed-name` do not exist)
    assert!(
        m.contains("hecaton_plugin_proxy_requests_total{plugin=\"unknown\",status=\"404\"} 2"),
        "{m}"
    );
    assert!(
        m.contains("hecaton_plugin_proxy_requests_total{plugin=\"unknown\",status=\"401\"} 5"),
        "{m}"
    );
    assert!(
        !m.contains("plugin=\"nope\"") && !m.contains("plugin=\"guessed-name\""),
        "an unauthenticated or unknown name never becomes a label: {m}"
    );
}
