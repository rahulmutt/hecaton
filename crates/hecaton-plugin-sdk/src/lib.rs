//! The plugin side of the host protocol (plugins spec §4.1, §7). Phase 1
//! ships `Env` and `hello`; observers, interceptors, actions, KV and attach
//! arrive with Phase 2. Nothing here reads the process environment except
//! `Env::from_process`, so plugins stay testable with an injected one.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use hecaton_api::{ErrorBody, HelloRequest, HelloResponse, PLUGIN_PROTOCOL};

/// The four `HECATON_*` variables the daemon sets through the nono profile
/// (plugins spec §5.1).
#[derive(Clone, PartialEq, Eq)]
pub struct Env {
    /// `http://127.0.0.1:<port>`, no trailing slash.
    pub api_url: String,
    pub name: String,
    pub token: String,
    pub scratch: PathBuf,
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("api_url", &self.api_url)
            .field("name", &self.name)
            .field("token", &"<redacted>")
            .field("scratch", &self.scratch)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SdkError {
    #[error("environment: {0} is not set")]
    MissingEnv(&'static str),
    #[error("daemon: {0}")]
    Transport(String),
    #[error("daemon: HTTP {status}: {message}")]
    Status { status: u16, message: String },
}

impl Env {
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, SdkError> {
        let var = |k: &'static str| {
            get(k)
                .filter(|v| !v.trim().is_empty())
                .ok_or(SdkError::MissingEnv(k))
        };
        Ok(Self {
            api_url: var("HECATON_API_URL")?
                .trim()
                .trim_end_matches('/')
                .to_string(),
            name: var("HECATON_PLUGIN_NAME")?,
            token: var("HECATON_PLUGIN_TOKEN")?,
            scratch: PathBuf::from(var("HECATON_PLUGIN_SCRATCH")?),
        })
    }

    /// The one place the SDK reads the real process environment.
    pub fn from_process() -> Result<Self, SdkError> {
        Self::from_env(|k| std::env::var(k).ok())
    }
}

/// Typed client for the daemon's plugin-host routes.
pub struct Host {
    env: Env,
    agent: ureq::Agent,
}

impl fmt::Debug for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Host").field("env", &self.env).finish()
    }
}

const TIMEOUT: Duration = Duration::from_secs(10);

impl Host {
    pub fn new(env: Env) -> Self {
        Self {
            env,
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(TIMEOUT))
                .http_status_as_error(false)
                .build()
                .into(),
        }
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    /// `POST /v1/plugin-host/hello` (plugins spec §4.1): announces the
    /// plugin's version and listen address; the daemon marks it `Ready`
    /// and answers with the daemon-level config.
    pub fn hello(&self, version: &str, listen: &str) -> Result<HelloResponse, SdkError> {
        let req = HelloRequest {
            name: self.env.name.clone(),
            version: version.to_string(),
            protocol: PLUGIN_PROTOCOL,
            listen: listen.to_string(),
        };
        let mut resp = self
            .agent
            .post(format!("{}/v1/plugin-host/hello", self.env.api_url))
            .header("Authorization", &format!("Bearer {}", self.env.token))
            .send_json(&req)
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        if !(200..300).contains(&status) {
            let message = serde_json::from_str::<ErrorBody>(&text)
                .map(|e| e.error)
                .unwrap_or(text);
            return Err(SdkError::Status { status, message });
        }
        serde_json::from_str(&text)
            .map_err(|e| SdkError::Transport(format!("bad hello reply: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| owned.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone())
    }

    const FULL: &[(&str, &str)] = &[
        ("HECATON_API_URL", "http://127.0.0.1:7643/"),
        ("HECATON_PLUGIN_NAME", "web"),
        ("HECATON_PLUGIN_TOKEN", "tok-secret"),
        ("HECATON_PLUGIN_SCRATCH", "/s/plugins/web/scratch"),
    ];

    #[test]
    fn env_reads_the_four_variables_and_names_the_missing_one() {
        let e = Env::from_env(env_of(FULL)).unwrap();
        assert_eq!(e.api_url, "http://127.0.0.1:7643", "trailing slash trimmed");
        assert_eq!(e.name, "web");
        assert_eq!(e.token, "tok-secret");
        assert_eq!(e.scratch, std::path::Path::new("/s/plugins/web/scratch"));
        for missing in [
            "HECATON_API_URL",
            "HECATON_PLUGIN_NAME",
            "HECATON_PLUGIN_TOKEN",
            "HECATON_PLUGIN_SCRATCH",
        ] {
            let vars: Vec<(&str, &str)> = FULL
                .iter()
                .copied()
                .filter(|(k, _)| *k != missing)
                .collect();
            let err = Env::from_env(env_of(&vars)).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("environment: {missing} is not set")
            );
        }
        let dbg = format!("{e:?}");
        assert!(
            dbg.contains("web") && !dbg.contains("tok-secret") && dbg.contains("<redacted>"),
            "{dbg}"
        );
        let host = Host::new(e);
        let dbg = format!("{host:?}");
        assert!(!dbg.contains("tok-secret"), "{dbg}");
    }

    /// One-request stub: captures the raw request, answers with a canned
    /// status and body.
    fn stub(status_line: &'static str, body: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = sock.read(&mut chunk).unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(idx) = text.find("\r\n\r\n") {
                    let len: usize = text[..idx]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= idx + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            tx.send(String::from_utf8_lossy(&buf).to_string()).unwrap();
            write!(
                sock,
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        (format!("http://{addr}"), rx)
    }

    fn host_at(url: &str) -> Host {
        let vars = [
            ("HECATON_API_URL", url),
            ("HECATON_PLUGIN_NAME", "web"),
            ("HECATON_PLUGIN_TOKEN", "tok-secret"),
            ("HECATON_PLUGIN_SCRATCH", "/s"),
        ];
        Host::new(Env::from_env(env_of(&vars)).unwrap())
    }

    #[test]
    fn hello_posts_the_request_with_the_bearer_and_returns_the_config() {
        let (url, rx) = stub("200 OK", r#"{"config":{"greeting":"hi"}}"#);
        let resp = host_at(&url).hello("0.1.0", "127.0.0.1:4321").unwrap();
        assert_eq!(resp.config["greeting"], "hi");
        let raw = rx.recv().unwrap();
        assert!(
            raw.starts_with("POST /v1/plugin-host/hello HTTP/1.1"),
            "{raw}"
        );
        assert!(
            raw.to_ascii_lowercase()
                .contains("authorization: bearer tok-secret"),
            "{raw}"
        );
        let body: serde_json::Value =
            serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "name": "web", "version": "0.1.0", "protocol": 1, "listen": "127.0.0.1:4321" })
        );
    }

    #[test]
    fn hello_reports_statuses_and_transport_failures() {
        let (url, _rx) = stub(
            "401 Unauthorized",
            r#"{"error":"unknown plugin or bad token"}"#,
        );
        let e = host_at(&url).hello("0.1.0", "127.0.0.1:1").unwrap_err();
        assert_eq!(
            e.to_string(),
            "daemon: HTTP 401: unknown plugin or bad token"
        );
        let e = host_at("http://127.0.0.1:1")
            .hello("0.1.0", "127.0.0.1:1")
            .unwrap_err();
        assert!(e.to_string().starts_with("daemon: "), "{e}");
        assert!(matches!(e, SdkError::Transport(_)));
    }
}
