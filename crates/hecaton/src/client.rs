//! The CLI's view of the daemon (Phase 3 spec §5): endpoint resolution and
//! typed calls over `ureq`. Plain HTTP on loopback (P3-1).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use hecaton_api::{DownQuery, ErrorBody, FleetRequest, FleetSummary};
use hecaton_core::FleetRecord;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::wiring::{layout_from_env, server_paths};

pub const NOT_RUNNING: &str = "daemon not running; run `hecaton serve -d`";

/// Per-request timeout: long enough for one reconcile pass to answer a
/// `GET`, short enough that a genuinely dead daemon fails fast.
const TIMEOUT: Duration = Duration::from_secs(30);

/// `--api-url`, then `$HECATON_API_URL`, then the running daemon's endpoint file.
pub fn resolve_endpoint(
    flag: Option<&str>,
    env: Option<&str>,
    endpoint_file: &Path,
) -> Result<String> {
    let url = match (flag, env) {
        (Some(f), _) => f.to_string(),
        (None, Some(e)) if !e.trim().is_empty() => e.to_string(),
        _ => hecaton_server::read_endpoint(endpoint_file)?.ok_or_else(|| anyhow!(NOT_RUNNING))?,
    };
    Ok(url.trim().trim_end_matches('/').to_string())
}

pub struct Client {
    base: String,
    token: String,
    agent: ureq::Agent,
    timeout: Duration,
}

impl Client {
    pub fn connect(api_url: Option<&str>) -> Result<Self> {
        let layout = layout_from_env()?;
        let paths = server_paths(&layout);
        let env = std::env::var("HECATON_API_URL").ok();
        let base = resolve_endpoint(api_url, env.as_deref(), &paths.endpoint())?;
        let token = std::fs::read_to_string(paths.token())
            .map(|t| t.trim().to_string())
            .map_err(|_| anyhow!("{NOT_RUNNING} (no token at {})", paths.token().display()))?;
        Ok(Self::new(base, token))
    }

    pub fn new(base: String, token: String) -> Self {
        Self::with_timeout(base, token, TIMEOUT)
    }

    /// As `new`, but with an explicit per-request timeout (tests use a
    /// short one to exercise the timeout path without waiting 30s).
    pub fn with_timeout(base: String, token: String, timeout: Duration) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token,
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(timeout))
                .http_status_as_error(false)
                .build()
                .into(),
            timeout,
        }
    }

    /// A timeout means the daemon is up but slow (spec §5: don't tell the
    /// operator to restart something that's running); every other
    /// transport-level failure means it could not be reached at all.
    fn transport_error(&self, e: ureq::Error) -> anyhow::Error {
        match e {
            ureq::Error::Timeout(_) => {
                anyhow!(
                    "daemon did not answer within {}s ({e})",
                    self.timeout.as_secs()
                )
            }
            _ => anyhow!("{NOT_RUNNING} ({e})"),
        }
    }

    fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Option<T>> {
        let url = format!("{}{path}", self.base);
        let auth = format!("Bearer {}", self.token);
        let sent = match (method, body) {
            ("GET", _) => self.agent.get(&url).header("Authorization", &auth).call(),
            ("DELETE", _) => self
                .agent
                .delete(&url)
                .header("Authorization", &auth)
                .call(),
            ("POST", Some(b)) => self
                .agent
                .post(&url)
                .header("Authorization", &auth)
                .send_json(b),
            ("PUT", Some(b)) => self
                .agent
                .put(&url)
                .header("Authorization", &auth)
                .send_json(b),
            _ => bail!("unsupported request {method} without a body"),
        };
        let mut resp = sent.map_err(|e| self.transport_error(e))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .context("cannot read the daemon's response")?;
        match status {
            200..=299 => Ok(Some(serde_json::from_str(&text).with_context(|| {
                format!("unexpected response from the daemon: {text}")
            })?)),
            404 => Ok(None),
            _ => {
                let message = serde_json::from_str::<ErrorBody>(&text)
                    .map(|e| e.error)
                    .unwrap_or(text);
                bail!("{message} (HTTP {status})")
            }
        }
    }

    fn must<T>(r: Result<Option<T>>, what: &str) -> Result<T> {
        r?.ok_or_else(|| anyhow!("{what} not found"))
    }

    pub fn create(&self, req: &FleetRequest) -> Result<FleetRecord> {
        Self::must(
            self.request("POST", "/v1/fleets", Some(&serde_json::to_value(req)?)),
            "fleet",
        )
    }

    pub fn update(&self, req: &FleetRequest) -> Result<FleetRecord> {
        Self::must(
            self.request(
                "PUT",
                &format!("/v1/fleets/{}", req.spec.name),
                Some(&serde_json::to_value(req)?),
            ),
            &format!("fleet {}", req.spec.name),
        )
    }

    pub fn get(&self, name: &str) -> Result<Option<FleetRecord>> {
        self.request("GET", &format!("/v1/fleets/{name}"), None)
    }

    pub fn list(&self) -> Result<Vec<FleetSummary>> {
        Self::must(self.request("GET", "/v1/fleets", None), "fleet list")
    }

    pub fn down(&self, name: &str, q: &DownQuery) -> Result<FleetRecord> {
        Self::must(
            self.request(
                "DELETE",
                &format!("/v1/fleets/{name}?{}", q.to_query_string()),
                None,
            ),
            &format!("fleet {name}"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_prefers_flag_then_env_then_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("endpoint");
        assert_eq!(
            resolve_endpoint(None, None, &file).unwrap_err().to_string(),
            NOT_RUNNING
        );
        std::fs::write(&file, "http://127.0.0.1:4000\n").unwrap();
        assert_eq!(
            resolve_endpoint(None, None, &file).unwrap(),
            "http://127.0.0.1:4000"
        );
        assert_eq!(
            resolve_endpoint(None, Some("http://127.0.0.1:5000/"), &file).unwrap(),
            "http://127.0.0.1:5000"
        );
        assert_eq!(
            resolve_endpoint(Some("http://127.0.0.1:6000"), Some("http://x"), &file).unwrap(),
            "http://127.0.0.1:6000"
        );
    }

    #[test]
    fn a_refused_connection_reads_as_daemon_not_running() {
        let c = Client::new("http://127.0.0.1:1".into(), "t".into());
        let e = c.list().unwrap_err().to_string();
        assert!(e.starts_with(NOT_RUNNING), "{e}");
    }

    #[test]
    fn a_slow_but_running_daemon_is_not_reported_as_not_running() {
        // Accepts the connection but never writes a response, so the
        // client's read times out instead of the connection failing.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(2));
                drop(stream);
            }
        });
        let c = Client::with_timeout(
            format!("http://{addr}"),
            "t".into(),
            Duration::from_millis(200),
        );
        let e = c.list().unwrap_err().to_string();
        assert!(e.contains("did not answer"), "{e}");
        assert!(!e.starts_with(NOT_RUNNING), "{e}");
    }
}
