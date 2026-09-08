//! Browser sessions for the proxied plugin routes (plugins spec §18.2): a
//! one-time login code minted for the admin, exchanged by the browser for
//! an in-memory session cookie. The admin token never enters the browser;
//! nothing here is persisted, so sessions die with the daemon.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;

use crate::auth::constant_time_eq;
use crate::vault::random_hex;

/// How long a login code may sit unused.
pub const CODE_TTL: Duration = Duration::from_secs(60);
/// How long a browser session lives.
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
pub const COOKIE: &str = "hecaton_session";
/// Every proxied path starts here; a login may only redirect below it, and
/// the cookie is scoped to it.
pub const MOUNT_PREFIX: &str = "/v1/plugins/";

#[derive(Default)]
struct Inner {
    /// code → expiry
    codes: HashMap<String, Instant>,
    /// session id → expiry
    sessions: HashMap<String, Instant>,
}

#[derive(Default)]
pub struct Sessions {
    inner: Mutex<Inner>,
}

impl Sessions {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn prune(inner: &mut Inner, now: Instant) {
        inner.codes.retain(|_, expiry| *expiry > now);
        inner.sessions.retain(|_, expiry| *expiry > now);
    }

    /// A fresh single-use code, 32 random bytes as hex, good for `CODE_TTL`.
    pub fn issue_code(&self) -> String {
        self.issue_code_at(Instant::now())
    }

    pub fn issue_code_at(&self, now: Instant) -> String {
        let mut inner = self.lock();
        Self::prune(&mut inner, now);
        let code = random_hex(32);
        inner.codes.insert(code.clone(), now + CODE_TTL);
        code
    }

    /// Exchanges a live code for a session id; the code is gone either way.
    pub fn redeem(&self, code: &str) -> Option<String> {
        self.redeem_at(code, Instant::now())
    }

    pub fn redeem_at(&self, code: &str, now: Instant) -> Option<String> {
        let mut inner = self.lock();
        Self::prune(&mut inner, now);
        // Few codes, compared in constant time each: a lookup by key would
        // leak through timing what a guesser is after.
        let key = inner
            .codes
            .keys()
            .find(|k| constant_time_eq(k.as_bytes(), code.as_bytes()))
            .cloned()?;
        inner.codes.remove(&key);
        let id = random_hex(32);
        inner.sessions.insert(id.clone(), now + SESSION_TTL);
        Some(id)
    }

    pub fn is_valid(&self, id: &str) -> bool {
        self.is_valid_at(id, Instant::now())
    }

    pub fn is_valid_at(&self, id: &str, now: Instant) -> bool {
        let mut inner = self.lock();
        Self::prune(&mut inner, now);
        inner
            .sessions
            .keys()
            .any(|k| constant_time_eq(k.as_bytes(), id.as_bytes()))
    }
}

/// The value of cookie `name` across every `Cookie` header, if any.
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|line| line.split(';'))
        .find_map(|pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k.trim() == name).then(|| v.trim().to_string())
        })
}

/// The `Set-Cookie` value: HttpOnly, never cross-site, scoped to the mount.
pub fn set_cookie(id: &str) -> String {
    format!("{COOKIE}={id}; HttpOnly; SameSite=Strict; Path={MOUNT_PREFIX}")
}

/// Whether a cookie-authenticated request came from the daemon's own
/// origin (§18.2): `Sec-Fetch-Site` when the browser sends it (every
/// current one does), else `Origin` — absent on a plain navigation, and
/// exactly `http://127.0.0.1:<port>` on a fetch or WebSocket from a page
/// the daemon served (`localhost` is another origin to a browser).
pub fn same_origin(headers: &HeaderMap, origin: &str) -> bool {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        return matches!(site.trim(), "same-origin" | "none");
    }
    match headers.get("origin").and_then(|v| v.to_str().ok()) {
        None => true,
        Some(o) => o.trim().trim_end_matches('/') == origin,
    }
}

/// Where a login may send the browser: a path under the mount made of
/// `[A-Za-z0-9/._~-]`, so it needs no encoding in a URL and cannot smuggle
/// a query, a fragment, a header or another host. A `.` or `..` segment is
/// rejected too — a browser normalizes it out of the `Location` path per
/// RFC 3986, and it could otherwise walk the redirect back out of the
/// mount. `None` for anything else.
pub fn login_target(to: Option<&str>) -> Option<String> {
    let to = to.unwrap_or(MOUNT_PREFIX);
    let charset_ok = to
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'~' | b'-'));
    let no_dot_segments = to.split('/').all(|s| !matches!(s, "." | ".."));
    (to.starts_with(MOUNT_PREFIX) && charset_ok && no_dot_segments).then(|| to.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::time::Duration;

    #[test]
    fn a_code_is_single_use_and_expires() {
        let s = Sessions::new();
        let t0 = Instant::now();
        let code = s.issue_code_at(t0);
        assert_eq!(code.len(), 64);
        assert_eq!(s.redeem_at("nope", t0), None);
        let id = s.redeem_at(&code, t0 + Duration::from_secs(30)).unwrap();
        assert_eq!(id.len(), 64);
        assert_ne!(id, code, "the session id is not the code");
        assert_eq!(s.redeem_at(&code, t0), None, "single use");
        let late = s.issue_code_at(t0);
        assert_eq!(
            s.redeem_at(&late, t0 + CODE_TTL + Duration::from_secs(1)),
            None,
            "expired"
        );
        assert!(s.is_valid_at(&id, t0 + Duration::from_secs(3600)));
        assert!(!s.is_valid_at("nope", t0));
        assert!(
            !s.is_valid_at(
                &id,
                t0 + Duration::from_secs(30) + SESSION_TTL + Duration::from_secs(1)
            ),
            "sessions expire"
        );
    }

    #[test]
    fn the_cookie_is_found_among_others_and_rendered_with_its_flags() {
        let mut h = HeaderMap::new();
        assert_eq!(cookie_value(&h, COOKIE), None);
        h.insert(
            "cookie",
            HeaderValue::from_static("a=1; hecaton_session=abc; b=2"),
        );
        assert_eq!(cookie_value(&h, COOKIE).as_deref(), Some("abc"));
        h.insert("cookie", HeaderValue::from_static("hecaton_session_x=zzz"));
        assert_eq!(cookie_value(&h, COOKIE), None, "prefix is not a match");
        assert_eq!(
            set_cookie("abc"),
            "hecaton_session=abc; HttpOnly; SameSite=Strict; Path=/v1/plugins/"
        );
    }

    #[test]
    fn same_origin_prefers_sec_fetch_site_then_origin() {
        let origin = "http://127.0.0.1:7643";
        let with = |pairs: &[(&'static str, &'static str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.insert(*k, HeaderValue::from_str(v).unwrap());
            }
            h
        };
        assert!(
            same_origin(&with(&[]), origin),
            "a navigation sends neither"
        );
        assert!(same_origin(
            &with(&[("sec-fetch-site", "same-origin")]),
            origin
        ));
        assert!(same_origin(&with(&[("sec-fetch-site", "none")]), origin));
        assert!(!same_origin(
            &with(&[("sec-fetch-site", "cross-site")]),
            origin
        ));
        assert!(!same_origin(
            &with(&[("sec-fetch-site", "same-site")]),
            origin
        ));
        assert!(same_origin(&with(&[("origin", origin)]), origin));
        assert!(same_origin(
            &with(&[("origin", "http://127.0.0.1:7643/")]),
            origin
        ));
        assert!(!same_origin(
            &with(&[("origin", "http://localhost:7643")]),
            origin
        ));
        assert!(!same_origin(
            &with(&[("origin", "http://evil.example")]),
            origin
        ));
        assert!(
            !same_origin(
                &with(&[("sec-fetch-site", "cross-site"), ("origin", origin)]),
                origin
            ),
            "sec-fetch-site wins when present"
        );
    }

    #[test]
    fn a_login_may_only_land_under_the_mount() {
        assert_eq!(login_target(None).as_deref(), Some("/v1/plugins/"));
        assert_eq!(
            login_target(Some("/v1/plugins/web/")).as_deref(),
            Some("/v1/plugins/web/")
        );
        assert_eq!(
            login_target(Some("/v1/plugins/web/agents/f/c/a")).as_deref(),
            Some("/v1/plugins/web/agents/f/c/a")
        );
        assert_eq!(
            login_target(Some("/v1/plugins/web/index.html")).as_deref(),
            Some("/v1/plugins/web/index.html")
        );
        for bad in [
            "/v1/fleets",
            "http://evil.example/v1/plugins/",
            "//evil.example/v1/plugins/",
            "/v1/plugins/web/?x=1",
            "/v1/plugins/web/\r\nSet-Cookie: x",
            "/v1/plugins",
            "",
            "/v1/plugins/../fleets",
            "/v1/plugins/..",
            "/v1/plugins/./web/",
        ] {
            assert_eq!(login_target(Some(bad)), None, "{bad:?}");
        }
    }
}
