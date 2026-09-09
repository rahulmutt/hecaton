//! The cached Matrix session (Spec G §5.2). It lives in sealed KV, which
//! the daemon encrypts at rest with its vault key, and the decision about
//! whether to use it is a pure function so "the password is read at most
//! once" is a tested property.

use std::fmt;

use hecaton_plugin_sdk::{Host, SdkError};
use serde::{Deserialize, Serialize};

use crate::config::{DaemonConfig, Secret};

/// The one sealed KV key this plugin writes.
pub const AUTH_KEY: &str = "auth";

/// What the operator should do when a cached session is unusable and no
/// password is configured.
pub const REVOKED: &str = "the cached session was rejected, most likely because its device was revoked; configure a password to log in again";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub homeserver: String,
    pub user_id: String,
    pub device_id: String,
    pub access_token: Secret,
    pub refresh_token: Option<Secret>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("homeserver", &self.homeserver)
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .finish()
    }
}

/// What to do at `configure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Reuse the cached session; the password is never read.
    Restore(Box<Session>),
    Login {
        password: Secret,
    },
}

/// A cached session is reused only when it belongs to the configured
/// homeserver and user. Otherwise a password is required, and its absence
/// is an error that says which of the two situations it is.
pub fn plan(cached: Option<Session>, config: &DaemonConfig) -> Result<Plan, String> {
    let usable =
        cached.filter(|s| s.homeserver == config.homeserver && s.user_id == config.user_id);
    if let Some(session) = usable {
        return Ok(Plan::Restore(Box::new(session)));
    }
    match &config.password {
        Some(password) => Ok(Plan::Login {
            password: password.clone(),
        }),
        None => Err(format!(
            "no cached session for {} at {}, and no password is configured: set `password` in the plugins.yaml config block, or add a `secrets.password` entry naming a 0600 file",
            config.user_id, config.homeserver
        )),
    }
}

pub async fn load(host: &Host) -> Result<Option<Session>, SdkError> {
    let Some(bytes) = host.kv_get(AUTH_KEY).await? else {
        return Ok(None);
    };
    match serde_json::from_slice(&bytes) {
        Ok(session) => Ok(Some(session)),
        Err(e) => {
            tracing::warn!("matrix: stored session is unreadable, logging in again: {e}");
            Ok(None)
        }
    }
}

pub async fn store(host: &Host, session: &Session) -> Result<(), SdkError> {
    let bytes = serde_json::to_vec(session)
        .map_err(|e| SdkError::Transport(format!("encode session: {e}")))?;
    host.kv_put(AUTH_KEY, &bytes, true).await
}

pub async fn clear(host: &Host) -> Result<(), SdkError> {
    host.kv_delete(AUTH_KEY).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use hecaton_plugin_sdk::testing::FakeHost;
    use serde_json::json;

    fn config(password: Option<&str>) -> DaemonConfig {
        let mut v = json!({ "homeserver": "https://h", "userId": "@hecaton:h" });
        if let Some(p) = password {
            v["password"] = json!(p);
        }
        crate::config::parse_daemon(&v).unwrap()
    }

    fn session() -> Session {
        Session {
            homeserver: "https://h".into(),
            user_id: "@hecaton:h".into(),
            device_id: "hecaton".into(),
            access_token: Secret::new("syt_tok"),
            refresh_token: Some(Secret::new("syr_ref")),
        }
    }

    #[test]
    fn a_matching_cached_session_is_restored_without_reading_the_password() {
        assert_eq!(
            plan(Some(session()), &config(Some("pw"))),
            Ok(Plan::Restore(Box::new(session())))
        );
        assert_eq!(
            plan(Some(session()), &config(None)),
            Ok(Plan::Restore(Box::new(session()))),
            "a cached session needs no password at all"
        );
    }

    #[test]
    fn a_changed_homeserver_or_user_discards_the_cached_session() {
        let mut c = config(Some("pw"));
        c.homeserver = "https://other".into();
        assert_eq!(
            plan(Some(session()), &c),
            Ok(Plan::Login {
                password: Secret::new("pw")
            })
        );
        let mut c = config(Some("pw"));
        c.user_id = "@other:h".into();
        assert!(matches!(plan(Some(session()), &c), Ok(Plan::Login { .. })));
    }

    #[test]
    fn no_cached_session_and_no_password_names_what_to_do() {
        let err = plan(None, &config(None)).unwrap_err();
        assert!(err.contains("no password"), "{err}");
        assert!(
            err.contains("secrets.password"),
            "names the file route: {err}"
        );

        let mut c = config(None);
        c.homeserver = "https://other".into();
        let err = plan(Some(session()), &c).unwrap_err();
        assert!(err.contains("https://other"), "names the mismatch: {err}");
        assert!(!err.contains("syt_tok"), "never the token: {err}");
    }

    #[test]
    fn debug_never_prints_a_token() {
        let text = format!("{:?}", session());
        assert!(
            !text.contains("syt_tok") && !text.contains("syr_ref"),
            "{text}"
        );
        assert!(text.contains("<redacted>"), "{text}");
        assert!(
            text.contains("hecaton"),
            "the device id is readable: {text}"
        );
    }

    #[tokio::test]
    async fn the_session_round_trips_through_sealed_kv() {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("matrix", std::path::Path::new("scratch"))).unwrap();
        assert_eq!(load(&host).await.unwrap(), None);

        store(&host, &session()).await.unwrap();
        assert_eq!(load(&host).await.unwrap(), Some(session()));
        assert_eq!(
            fake.kv().get(AUTH_KEY).map(|(_, secret)| *secret),
            Some(true),
            "the record is stored sealed"
        );

        clear(&host).await.unwrap();
        assert_eq!(load(&host).await.unwrap(), None);
    }
}
