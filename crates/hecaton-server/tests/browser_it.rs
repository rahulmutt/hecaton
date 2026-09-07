//! Plugins spec §18.2 through a real listener: login codes, the session
//! cookie, the same-origin rule, and (Task 3) the proxied mount.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::json;
use support::world;

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
    assert_eq!(
        (s, text.as_str()),
        (404, "unknown or expired login code"),
        "single use"
    );
    let (s, _, _) = w.api.raw("GET", "/v1/login/nope", &[], None);
    assert_eq!(s, 404);
    let (s, _, text) = w.api.raw("GET", "/v1/login/nope?to=/v1/fleets", &[], None);
    assert_eq!(
        (s, text.as_str()),
        (400, "to: must be a path under /v1/plugins/")
    );
}
