//! The plugin → daemon half (plugins spec §4.1): one method per route,
//! bearer from `Env`, loopback only.

use std::fmt;
use std::time::Duration;

use axum::http::HeaderValue;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use hecaton_api::{
    ErrorBody, FleetRecord, HelloRequest, HelloResponse, KvKeys, PLUGIN_PROTOCOL, PluginAction,
    ResizeFrame,
};
use serde::de::DeserializeOwned;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::{Env, SdkError};

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct Host {
    env: Env,
    http: reqwest::Client,
}

impl fmt::Debug for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Host").field("env", &self.env).finish()
    }
}

/// Percent-encodes everything the predicate does not call safe
/// (`reqwest`'s `query` feature is not enabled — plugins spec §16.1: no
/// extra dependency weight for it).
fn encode(s: &str, safe: impl Fn(u8) -> bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if safe(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'~' | b'-')
}

/// Everything outside `[A-Za-z0-9._~/-]`, for building query strings.
fn urlencode(s: &str) -> String {
    encode(s, |b| unreserved(b) || matches!(b, b'.' | b'/'))
}

/// A kv key as one path segment: `.` and `/` are escaped too. The URL
/// parser resolves `.` and `..` segments (their `%2e` spellings
/// included) while it builds the request, so an unescaped `../x` would
/// leave for a different route instead of collecting the daemon's
/// `kv: invalid key` (plugins spec §4.1).
fn path_encode(s: &str) -> String {
    encode(s, unreserved)
}

impl Host {
    pub fn new(env: Env) -> Result<Self, SdkError> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        Ok(Self { env, http })
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v1/plugin-host/{path}", self.env.api_url)
    }

    /// Sends with the bearer; `Ok((status, bytes))` for any status.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<(u16, Vec<u8>), SdkError> {
        let resp = req
            .bearer_auth(&self.env.token)
            .send()
            .await
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        Ok((status, bytes.to_vec()))
    }

    fn status_error(status: u16, bytes: &[u8]) -> SdkError {
        let text = String::from_utf8_lossy(bytes).trim().to_string();
        let message = serde_json::from_slice::<ErrorBody>(bytes)
            .map(|e| e.error)
            .unwrap_or(text);
        SdkError::Status { status, message }
    }

    /// 2xx → parsed body; anything else → `Status`.
    async fn json<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T, SdkError> {
        let (status, bytes) = self.send(req).await?;
        if !(200..300).contains(&status) {
            return Err(Self::status_error(status, &bytes));
        }
        serde_json::from_slice(&bytes).map_err(|e| SdkError::Transport(format!("bad reply: {e}")))
    }

    /// `POST /v1/plugin-host/hello` (plugins spec §4.1): announces the
    /// plugin's version and listen address; the daemon marks it `Ready`
    /// and answers with the daemon-level config.
    pub async fn hello(&self, version: &str, listen: &str) -> Result<HelloResponse, SdkError> {
        let req = HelloRequest {
            name: self.env.name.clone(),
            version: version.to_string(),
            protocol: PLUGIN_PROTOCOL,
            listen: listen.to_string(),
        };
        self.json(self.http.post(self.url("hello")).json(&req))
            .await
    }

    pub async fn fleets(&self) -> Result<Vec<FleetRecord>, SdkError> {
        self.json(self.http.get(self.url("fleets"))).await
    }

    pub async fn fleet(&self, name: &str) -> Result<Option<FleetRecord>, SdkError> {
        let (status, bytes) = self
            .send(self.http.get(self.url(&format!("fleets/{name}"))))
            .await?;
        match status {
            200..=299 => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| SdkError::Transport(format!("bad reply: {e}"))),
            404 => Ok(None),
            _ => Err(Self::status_error(status, &bytes)),
        }
    }

    pub async fn action(&self, agent: &str, action: &PluginAction) -> Result<(), SdkError> {
        self.json::<serde_json::Value>(
            self.http
                .post(self.url(&format!("agents/{agent}/actions")))
                .json(action),
        )
        .await
        .map(|_| ())
    }

    pub async fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>, SdkError> {
        let (status, bytes) = self
            .send(self.http.get(self.url(&format!("kv/{}", path_encode(key)))))
            .await?;
        match status {
            200..=299 => Ok(Some(bytes)),
            404 => Ok(None),
            _ => Err(Self::status_error(status, &bytes)),
        }
    }

    pub async fn kv_put(&self, key: &str, bytes: &[u8], secret: bool) -> Result<(), SdkError> {
        let req = self
            .http
            .put(self.url(&format!("kv/{}?secret={secret}", path_encode(key))))
            .header("content-type", "application/octet-stream")
            .body(bytes.to_vec());
        self.json::<serde_json::Value>(req).await.map(|_| ())
    }

    pub async fn kv_delete(&self, key: &str) -> Result<(), SdkError> {
        self.json::<serde_json::Value>(
            self.http
                .delete(self.url(&format!("kv/{}", path_encode(key)))),
        )
        .await
        .map(|_| ())
    }

    pub async fn kv_list(&self, prefix: &str) -> Result<Vec<String>, SdkError> {
        let keys: KvKeys = self
            .json(
                self.http
                    .get(self.url(&format!("kv?prefix={}", urlencode(prefix)))),
            )
            .await?;
        Ok(keys.keys)
    }

    /// `ws://` twin of `url`: the streams of §18.4.
    fn ws_url(&self, path: &str) -> String {
        format!(
            "{}/v1/plugin-host/{path}",
            self.env.api_url.replacen("http://", "ws://", 1)
        )
    }

    /// Opens one of the daemon's WebSocket routes with the bearer; a
    /// refused handshake is the daemon's status and error, as for HTTP.
    async fn connect(&self, path: &str) -> Result<Socket, SdkError> {
        let mut req = self
            .ws_url(path)
            .into_client_request()
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        let bearer = HeaderValue::from_str(&format!("Bearer {}", self.env.token))
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        req.headers_mut().insert("authorization", bearer);
        match connect_async(req).await {
            Ok((socket, _)) => Ok(socket),
            Err(tungstenite::Error::Http(resp)) => {
                let status = resp.status().as_u16();
                let body = resp.body().clone().unwrap_or_default();
                Err(Self::status_error(status, &body))
            }
            Err(e) => Err(SdkError::Transport(e.to_string())),
        }
    }

    /// `GET agents/{id}/attach` (WS): a terminal on the agent's window
    /// (plugin-protocol §3 "Streams"). Needs the `attach` capability and
    /// an active pair.
    pub async fn attach(&self, agent: &str) -> Result<Attach, SdkError> {
        let (tx, rx) = self
            .connect(&format!("agents/{agent}/attach"))
            .await?
            .split();
        Ok(Attach {
            rx: AttachRead { rx },
            tx: AttachWrite { tx },
        })
    }

    /// `GET fleets/watch` (WS): the complete fleets list on every change.
    /// Connects lazily and reconnects forever; drop it to stop.
    pub fn watch_fleets(&self) -> FleetWatch {
        FleetWatch {
            host: self.clone(),
            socket: None,
            backoff: BACKOFF_MIN,
        }
    }
}

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

fn transport(e: tungstenite::Error) -> SdkError {
    SdkError::Transport(e.to_string())
}

/// The reading half of an attach: terminal output.
pub struct AttachRead {
    rx: SplitStream<Socket>,
}

impl AttachRead {
    /// The next chunk of output; `None` once the daemon closed the stream.
    pub async fn read(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.rx.next().await {
                Some(Ok(Message::Binary(bytes))) => return Some(bytes.to_vec()),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
                Some(Ok(_)) => {}
            }
        }
    }
}

/// The writing half of an attach: keystrokes and resizes.
pub struct AttachWrite {
    tx: SplitSink<Socket, Message>,
}

impl AttachWrite {
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), SdkError> {
        self.tx
            .send(Message::Binary(bytes.to_vec().into()))
            .await
            .map_err(transport)
    }

    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<(), SdkError> {
        let text = serde_json::to_string(&ResizeFrame::new(cols, rows))
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        self.tx
            .send(Message::Text(text.into()))
            .await
            .map_err(transport)
    }

    pub async fn close(mut self) {
        let _ = self.tx.send(Message::Close(None)).await;
        let _ = self.tx.close().await;
    }
}

/// A terminal on an agent's window; `split` for a bridge that reads and
/// writes concurrently.
pub struct Attach {
    rx: AttachRead,
    tx: AttachWrite,
}

impl fmt::Debug for Attach {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Attach")
    }
}

impl Attach {
    pub async fn read(&mut self) -> Option<Vec<u8>> {
        self.rx.read().await
    }
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), SdkError> {
        self.tx.write(bytes).await
    }
    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<(), SdkError> {
        self.tx.resize(cols, rows).await
    }
    pub async fn close(self) {
        self.tx.close().await;
    }
    pub fn split(self) -> (AttachRead, AttachWrite) {
        (self.rx, self.tx)
    }
}

/// `fleets/watch` as a stream of complete lists (§18.4). Every frame is
/// the whole list, so a reconnect simply yields it again.
pub struct FleetWatch {
    host: Host,
    socket: Option<Socket>,
    backoff: Duration,
}

impl FleetWatch {
    /// The next complete list. Never ends: a dropped socket is reconnected
    /// with a 1–10 s backoff; drop the watch to stop.
    pub async fn next(&mut self) -> Vec<FleetRecord> {
        loop {
            if self.socket.is_none() {
                match self.host.connect("fleets/watch").await {
                    Ok(s) => {
                        self.socket = Some(s);
                        self.backoff = BACKOFF_MIN;
                    }
                    Err(e) => {
                        eprintln!(
                            "{}: fleets/watch: {e}; retrying in {:?}",
                            self.host.env.name, self.backoff
                        );
                        tokio::time::sleep(self.backoff).await;
                        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
                        continue;
                    }
                }
            }
            let Some(socket) = self.socket.as_mut() else {
                continue;
            };
            match socket.next().await {
                Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
                    Ok(list) => return list,
                    Err(e) => eprintln!("{}: fleets/watch: bad frame: {e}", self.host.env.name),
                },
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => self.socket = None,
                Some(Ok(_)) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeHost;
    use hecaton_api::{FleetRecord, FleetSpec, PluginAction};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn record(name: &str) -> FleetRecord {
        FleetRecord::new(FleetSpec {
            name: name.into(),
            crews: BTreeMap::new(),
        })
    }

    #[test]
    fn urlencode_escapes_everything_outside_the_safe_set() {
        assert_eq!(urlencode("a b/c"), "a%20b/c");
    }

    #[test]
    fn a_kv_key_travels_as_one_segment_so_dots_reach_the_daemon() {
        assert_eq!(path_encode("state/f/c/a"), "state%2Ff%2Fc%2Fa");
        let url: reqwest::Url = format!("http://h/v1/plugin-host/kv/{}", path_encode("../x"))
            .parse()
            .unwrap();
        assert_eq!(url.path(), "/v1/plugin-host/kv/%2E%2E%2Fx");
    }

    #[tokio::test]
    #[allow(clippy::bool_assert_comparison)]
    async fn every_route_round_trips_through_the_fake_host() {
        let fake =
            FakeHost::start("tok", json!({ "greeting": "hi" }), vec![record("payments")]).await;
        let host = Host::new(fake.env("flow", std::path::Path::new("/s"))).unwrap();
        let hello = host.hello("0.1.0", "127.0.0.1:4321").await.unwrap();
        assert_eq!(hello.config["greeting"], "hi");
        let seen = fake.hellos();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            (
                seen[0].name.as_str(),
                seen[0].protocol,
                seen[0].listen.as_str()
            ),
            ("flow", 1, "127.0.0.1:4321")
        );

        let fleets = host.fleets().await.unwrap();
        assert_eq!(fleets.len(), 1);
        assert_eq!(fleets[0].name(), "payments");
        assert_eq!(
            host.fleet("payments").await.unwrap().unwrap().name(),
            "payments"
        );
        assert_eq!(host.fleet("nope").await.unwrap(), None);

        host.action("payments/backend/bob", &PluginAction::Restart)
            .await
            .unwrap();
        assert_eq!(
            fake.actions(),
            vec![("payments/backend/bob".to_string(), PluginAction::Restart)]
        );

        assert_eq!(host.kv_get("state/x").await.unwrap(), None);
        host.kv_put("state/x", b"working", false).await.unwrap();
        host.kv_put("secret/t", b"\x00\x01", true).await.unwrap();
        assert_eq!(
            host.kv_get("state/x").await.unwrap().as_deref(),
            Some(&b"working"[..])
        );
        assert_eq!(
            host.kv_get("secret/t").await.unwrap().as_deref(),
            Some(&b"\x00\x01"[..])
        );
        assert_eq!(fake.kv()["secret/t"].1, true, "the secret flag was sent");
        assert_eq!(
            host.kv_list("state/").await.unwrap(),
            vec!["state/x".to_string()]
        );
        assert_eq!(host.kv_list("").await.unwrap().len(), 2);
        host.kv_delete("state/x").await.unwrap();
        assert_eq!(host.kv_get("state/x").await.unwrap(), None);
    }

    #[tokio::test]
    async fn attach_streams_bytes_and_resizes_through_the_fake_host() {
        let fake = FakeHost::start("tok", json!({}), vec![]).await;
        let host = Host::new(fake.env("web", std::path::Path::new("/s"))).unwrap();
        let mut a = host.attach("payments/backend/bob").await.unwrap();
        a.write(b"ls\n").await.unwrap();
        assert_eq!(
            a.read().await.as_deref(),
            Some(&b"ls\n"[..]),
            "the fake echoes"
        );
        a.resize(120, 40).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while fake.resizes().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            fake.resizes(),
            vec![(
                "payments/backend/bob".to_string(),
                json!({ "resize": { "cols": 120, "rows": 40 } })
            )]
        );
        let (mut rd, mut wr) = a.split();
        wr.write(b"x").await.unwrap();
        assert_eq!(rd.read().await.as_deref(), Some(&b"x"[..]));
        wr.close().await;
        assert_eq!(rd.read().await, None, "closed");
        assert_eq!(fake.attaches(), vec!["payments/backend/bob".to_string()]);
        let mut env = fake.env("web", std::path::Path::new("/s"));
        env.token = "wrong".into();
        let e = Host::new(env).unwrap().attach("f/c/a").await.unwrap_err();
        assert_eq!(
            e.to_string(),
            "daemon: HTTP 401: unknown plugin or bad token"
        );
    }

    #[tokio::test]
    async fn watch_fleets_yields_the_current_list_then_every_change_and_reconnects() {
        let fake = FakeHost::start("tok", json!({}), vec![record("payments")]).await;
        let host = Host::new(fake.env("web", std::path::Path::new("/s"))).unwrap();
        let mut watch = host.watch_fleets();
        let first = watch.next().await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].name(), "payments");
        fake.set_fleets(vec![record("payments"), record("billing")]);
        let second = watch.next().await;
        assert_eq!(second.len(), 2);
        fake.set_fleets(vec![]);
        assert!(watch.next().await.is_empty());
        // a dropped socket: the next call reconnects and yields the list again
        fake.drop_watchers();
        fake.set_fleets(vec![record("again")]);
        let after = tokio::time::timeout(std::time::Duration::from_secs(5), watch.next())
            .await
            .expect("reconnected");
        assert_eq!(after[0].name(), "again");
    }

    #[tokio::test]
    async fn statuses_and_transport_failures_are_reported() {
        let fake = FakeHost::start("tok", json!({}), vec![]).await;
        let mut env = fake.env("flow", std::path::Path::new("/s"));
        env.token = "wrong".into();
        let e = Host::new(env)
            .unwrap()
            .hello("0.1.0", "127.0.0.1:1")
            .await
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "daemon: HTTP 401: unknown plugin or bad token"
        );
        let mut env = fake.env("flow", std::path::Path::new("/s"));
        env.api_url = "http://127.0.0.1:1".into();
        let e = Host::new(env)
            .unwrap()
            .hello("0.1.0", "127.0.0.1:1")
            .await
            .unwrap_err();
        assert!(matches!(e, SdkError::Transport(_)), "{e}");
        let mut env = fake.env("flow", std::path::Path::new("/s"));
        env.token = "s3cret-value".into();
        let dbg = format!("{:?}", Host::new(env).unwrap());
        assert!(
            !dbg.contains("s3cret-value") && dbg.contains("<redacted>"),
            "{dbg}"
        );
    }
}
