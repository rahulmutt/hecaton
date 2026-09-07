//! The plugin → daemon half (plugins spec §4.1): one method per route,
//! bearer from `Env`, loopback only.

use std::fmt;
use std::time::Duration;

use hecaton_api::{
    ErrorBody, FleetRecord, HelloRequest, HelloResponse, KvKeys, PLUGIN_PROTOCOL, PluginAction,
};
use serde::de::DeserializeOwned;

use crate::{Env, SdkError};

const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Host {
    env: Env,
    http: reqwest::Client,
}

impl fmt::Debug for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Host").field("env", &self.env).finish()
    }
}

/// Percent-encodes everything outside `[A-Za-z0-9._~/-]`, for building
/// query strings by hand (`reqwest`'s `query` feature is not enabled —
/// plugins spec §16.1: no extra dependency weight for it).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'~' | b'/' | b'-' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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
            .send(self.http.get(self.url(&format!("kv/{key}"))))
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
            .put(self.url(&format!("kv/{key}?secret={secret}")))
            .header("content-type", "application/octet-stream")
            .body(bytes.to_vec());
        self.json::<serde_json::Value>(req).await.map(|_| ())
    }

    pub async fn kv_delete(&self, key: &str) -> Result<(), SdkError> {
        self.json::<serde_json::Value>(self.http.delete(self.url(&format!("kv/{key}"))))
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
        let dbg = format!(
            "{:?}",
            Host::new(fake.env("flow", std::path::Path::new("/s"))).unwrap()
        );
        assert!(!dbg.contains("tok"), "{dbg}");
    }
}
