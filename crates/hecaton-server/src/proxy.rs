//! The reverse-proxied plugin mount (plugins spec §6, §18.1, §18.2):
//! `/v1/plugins/<name>/…` → `http://<listen>/v1/routes/…` on hyper's
//! legacy client. Bodies in are capped, bodies out stream, the daemon's
//! own credentials and the hop-by-hop headers never cross, and a 101 is
//! upgraded on both sides and copied byte for byte — the proxy never
//! parses a WebSocket frame.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};

use crate::api::ApiError;
use crate::plugins::PluginAddr;

pub type HttpClient = Client<HttpConnector, Body>;

/// Proxied request bodies are capped like every other body.
pub const MAX_BODY: usize = 1 << 20;
/// Tells a plugin where it is mounted, so it can build links.
pub const FORWARDED_PREFIX: &str = "x-hecaton-forwarded-prefix";

/// Never forwarded on a request: the daemon's own credentials, the host
/// (hyper sets it from the upstream URI), and the hop-by-hop set of RFC
/// 9110 §7.6.1 — `connection` and `upgrade` excepted on an upgrade
/// request, which is exactly what they are for.
pub(crate) const DROPPED: [&str; 9] = [
    "authorization",
    "cookie",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

/// Never forwarded on a response; `connection`/`upgrade` stay on a 101.
const DROPPED_RESPONSE: [&str; 6] = [
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

pub fn client() -> HttpClient {
    Client::builder(TokioExecutor::new()).build_http()
}

pub fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(header::UPGRADE)
}

fn filtered(src: &HeaderMap, dropped: &[&str], keep_upgrade: bool) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in src {
        let name = k.as_str();
        if dropped.contains(&name) {
            continue;
        }
        if !keep_upgrade && (name == "connection" || name == "upgrade") {
            continue;
        }
        out.append(k.clone(), v.clone());
    }
    out
}

pub fn forwarded_headers(src: &HeaderMap) -> HeaderMap {
    filtered(src, &DROPPED, is_upgrade(src))
}

pub fn response_headers(src: &HeaderMap, upgraded: bool) -> HeaderMap {
    filtered(src, &DROPPED_RESPONSE, upgraded)
}

/// `http://<listen>/v1/routes` for an empty `rest` (axum answers a nested
/// router's `/` there, not at `/v1/routes/`), else `/v1/routes/<rest>`,
/// with the query string as it came.
pub fn upstream_uri(listen: &str, rest: &str, query: Option<&str>) -> Result<Uri, String> {
    let path = if rest.is_empty() {
        "/v1/routes".to_string()
    } else {
        format!("/v1/routes/{rest}")
    };
    let query = query.map(|q| format!("?{q}")).unwrap_or_default();
    format!("http://{listen}{path}{query}")
        .parse()
        .map_err(|e: axum::http::uri::InvalidUri| e.to_string())
}

/// Forwards one request to the plugin and answers with its response. On a
/// 101 both sides are upgraded and copied until either closes; the copy
/// runs on its own task, the 101 goes back to the client at once.
pub async fn forward(
    client: &HttpClient,
    addr: &PluginAddr,
    name: &str,
    rest: &str,
    mut req: Request,
) -> Response {
    // Taken before the request is consumed: hyper stores the client
    // side's upgrade handle in the request's extensions.
    let downstream = hyper::upgrade::on(&mut req);
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => {
            return ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "body exceeds 1 MiB")
                .into_response();
        }
    };
    let uri = match upstream_uri(&addr.listen, rest, parts.uri.query()) {
        Ok(u) => u,
        Err(e) => return ApiError::new(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let mut headers = forwarded_headers(&parts.headers);
    let prefix = format!("/v1/plugins/{name}");
    let (Ok(prefix), Ok(bearer)) = (
        HeaderValue::from_str(&prefix),
        HeaderValue::from_str(&format!("Bearer {}", addr.token)),
    ) else {
        return ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "proxy: bad header value")
            .into_response();
    };
    headers.insert(HeaderName::from_static(FORWARDED_PREFIX), prefix);
    headers.insert(header::AUTHORIZATION, bearer);
    let mut upstream = match Request::builder()
        .method(parts.method)
        .uri(uri)
        .body(Body::from(bytes))
    {
        Ok(r) => r,
        Err(e) => {
            return ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("proxy: {e}"))
                .into_response();
        }
    };
    *upstream.headers_mut() = headers;
    let mut resp = match client.request(upstream).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(plugin = %name, "proxy: {e}");
            return ApiError::new(StatusCode::BAD_GATEWAY, format!("plugin {name:?}: {e}"))
                .into_response();
        }
    };
    let upgraded = resp.status() == StatusCode::SWITCHING_PROTOCOLS;
    if upgraded {
        let upstream = hyper::upgrade::on(&mut resp);
        let plugin = name.to_string();
        tokio::spawn(async move {
            match (downstream.await, upstream.await) {
                (Ok(a), Ok(b)) => {
                    let (mut a, mut b) = (TokioIo::new(a), TokioIo::new(b));
                    if let Err(e) = tokio::io::copy_bidirectional(&mut a, &mut b).await {
                        tracing::debug!(plugin = %plugin, "proxied stream ended: {e}");
                    }
                }
                (Err(e), _) | (_, Err(e)) => {
                    tracing::debug!(plugin = %plugin, "upgrade failed: {e}");
                }
            }
        });
    }
    let (mut parts, body) = resp.into_parts();
    parts.headers = response_headers(&parts.headers, upgraded);
    Response::from_parts(parts, Body::new(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use proptest::prelude::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn credentials_host_and_hop_by_hop_headers_never_cross() {
        let src = headers(&[
            ("authorization", "Bearer admin"),
            ("cookie", "hecaton_session=x"),
            ("host", "127.0.0.1:7643"),
            ("keep-alive", "timeout=5"),
            ("proxy-authorization", "x"),
            ("te", "trailers"),
            ("trailer", "x"),
            ("transfer-encoding", "chunked"),
            ("connection", "keep-alive"),
            ("upgrade", "h2c"),
            ("accept", "text/html"),
            ("x-custom", "1"),
        ]);
        let out = forwarded_headers(&headers(
            &src.iter()
                .filter(|(k, _)| k.as_str() != "upgrade")
                .map(|(k, v)| (k.as_str(), v.to_str().unwrap()))
                .collect::<Vec<_>>(),
        ));
        let names: Vec<&str> = out.keys().map(|k| k.as_str()).collect();
        assert_eq!(
            names,
            vec!["accept", "x-custom"],
            "no upgrade: connection dropped too"
        );
        let out = forwarded_headers(&src);
        let mut names: Vec<&str> = out.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["accept", "connection", "upgrade", "x-custom"],
            "an upgrade keeps its two hop-by-hop headers"
        );
        let resp = response_headers(
            &headers(&[
                ("set-cookie", "a=1"),
                ("transfer-encoding", "chunked"),
                ("connection", "close"),
                ("content-type", "text/html"),
            ]),
            false,
        );
        let mut names: Vec<&str> = resp.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["content-type", "set-cookie"]);
        let resp = response_headers(
            &headers(&[
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-accept", "x"),
            ]),
            true,
        );
        assert_eq!(resp.len(), 3, "a 101 keeps its upgrade headers");
    }

    #[test]
    fn the_upstream_uri_lands_under_v1_routes() {
        assert_eq!(
            upstream_uri("127.0.0.1:4000", "", None)
                .unwrap()
                .to_string(),
            "http://127.0.0.1:4000/v1/routes"
        );
        assert_eq!(
            upstream_uri("127.0.0.1:4000", "agents/f/c/a/ws", Some("cols=80"))
                .unwrap()
                .to_string(),
            "http://127.0.0.1:4000/v1/routes/agents/f/c/a/ws?cols=80"
        );
        assert!(upstream_uri("127.0.0.1:4000", "a b", None).is_err());
    }

    proptest! {
        /// Whatever comes in, the daemon's credentials and the hop-by-hop
        /// set never go out, and connection/upgrade go out only together
        /// with an upgrade.
        #[test]
        fn forwarded_headers_never_leak(
            names in proptest::collection::vec("[a-z-]{1,12}", 0..12),
            upgrade in proptest::bool::ANY,
        ) {
            let mut pairs: Vec<(String, String)> = names.iter().map(|n| (n.clone(), "v".to_string())).collect();
            for n in ["authorization", "cookie", "host", "connection", "te", "transfer-encoding"] {
                pairs.push((n.to_string(), "v".to_string()));
            }
            if upgrade {
                pairs.push(("upgrade".to_string(), "websocket".to_string()));
            }
            let src = headers(&pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>());
            let out = forwarded_headers(&src);
            for k in out.keys() {
                prop_assert!(!DROPPED.contains(&k.as_str()), "{k}");
            }
            prop_assert_eq!(out.contains_key("connection"), upgrade);
            prop_assert_eq!(out.contains_key("upgrade"), upgrade);
        }
    }
}
