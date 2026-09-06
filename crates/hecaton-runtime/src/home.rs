//! The agent's `$HOME` (Phase 2 spec §4.2 step 2): Claude settings with
//! hecaton's hooks, credentials, a seeded `.claude.json`, gh hosts.

use std::path::Path;

use hecaton_api::CredentialBundle;
use hecaton_core::{AgentId, HookTarget, MaterializeError};
use serde_json::{Map, Value, json};

use crate::fsutil::{ensure_dir, write_atomic};
use crate::layout::AgentPaths;

/// Every Claude Code hook event hecaton listens to. Kept as one list so the
/// daemon and the settings writer agree.
pub const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
    "SubagentStop",
    "PreCompact",
];

pub const REDACTED: &str = "<redacted>";

/// User settings plus the hecaton-owned `hooks` block. Any user `hooks` key
/// was rejected at validation; this overwrites unconditionally anyway.
pub fn render_settings(user: &Value, id: &AgentId, hooks: &HookTarget) -> Value {
    let mut settings = match user {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    let url = format!(
        "{}/v1/agents/{}/{}/{}/events",
        hooks.url.trim_end_matches('/'),
        id.fleet,
        id.crew,
        id.agent
    );
    let entry = json!([{ "hooks": [{ "type": "http", "url": url, "headers": { "Authorization": format!("Bearer {}", hooks.secret) } }] }]);
    let block: Map<String, Value> = HOOK_EVENTS
        .iter()
        .map(|e| (e.to_string(), entry.clone()))
        .collect();
    settings.insert("hooks".to_string(), Value::Object(block));
    Value::Object(settings)
}

pub fn render_hosts_yml(token: &str) -> String {
    format!("github.com:\n    oauth_token: {token}\n    git_protocol: https\n")
}

/// Seed that suppresses first-run prompts, overlaid with the account fields
/// the bundle carried (`oauthAccount`, `hasCompletedOnboarding`).
pub fn render_claude_json(account: Option<&Value>) -> Value {
    let mut m = Map::new();
    m.insert("hasCompletedOnboarding".to_string(), json!(true));
    if let Some(Value::Object(a)) = account {
        for (k, v) in a {
            m.insert(k.clone(), v.clone());
        }
    }
    Value::Object(m)
}

pub struct HomeInputs<'a> {
    pub settings: &'a Value,
    pub creds: &'a CredentialBundle,
    pub hooks: &'a HookTarget,
    pub with_gh: bool,
    pub redact_credentials: bool,
}

fn io_err(id: &AgentId, path: &Path, e: std::io::Error) -> MaterializeError {
    MaterializeError::Io {
        id: id.to_string(),
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

pub fn write_home(
    id: &AgentId,
    paths: &AgentPaths,
    inputs: &HomeInputs,
) -> Result<(), MaterializeError> {
    for d in [
        paths.home.clone(),
        paths.claude_dir(),
        paths.gh_dir(),
        paths.xdg_data(),
        paths.xdg_state(),
        paths.xdg_cache(),
        paths.nono_home.clone(),
        paths.logs.clone(),
    ] {
        ensure_dir(&d).map_err(|e| io_err(id, &d, e))?;
    }
    let pretty = |v: &Value| serde_json::to_vec_pretty(v).unwrap_or_default();

    let settings_path = paths.claude_dir().join("settings.json");
    write_atomic(
        &settings_path,
        &pretty(&render_settings(inputs.settings, id, inputs.hooks)),
        0o644,
    )
    .map_err(|e| io_err(id, &settings_path, e))?;

    let creds_path = paths.claude_dir().join(".credentials.json");
    match (&inputs.creds.claude_credentials, inputs.redact_credentials) {
        (Some(_), true) => write_atomic(&creds_path, &pretty(&json!(REDACTED)), 0o600),
        (Some(c), false) => write_atomic(&creds_path, &pretty(c), 0o600),
        (None, _) => Ok(()),
    }
    .map_err(|e| io_err(id, &creds_path, e))?;

    let claude_json = paths.home.join(".claude.json");
    write_atomic(
        &claude_json,
        &pretty(&render_claude_json(inputs.creds.claude_account.as_ref())),
        0o600,
    )
    .map_err(|e| io_err(id, &claude_json, e))?;

    if inputs.with_gh
        && let Some(token) = &inputs.creds.gh_token
    {
        let hosts = paths.gh_dir().join("hosts.yml");
        let token = if inputs.redact_credentials {
            REDACTED
        } else {
            token.as_str()
        };
        write_atomic(&hosts, render_hosts_yml(token).as_bytes(), 0o600)
            .map_err(|e| io_err(id, &hosts, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::StateLayout;
    use std::os::unix::fs::PermissionsExt;

    fn id() -> AgentId {
        "payments/backend/alice".parse().unwrap()
    }
    fn hooks() -> HookTarget {
        HookTarget {
            url: "https://127.0.0.1:7643/".into(),
            secret: "s3".into(),
        }
    }

    #[test]
    fn settings_get_a_hooks_block_for_every_event() {
        let v = render_settings(
            &json!({ "model": "opus", "hooks": { "Stop": [] } }),
            &id(),
            &hooks(),
        );
        assert_eq!(v["model"], "opus");
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), HOOK_EVENTS.len());
        let h = &hooks["PreToolUse"][0]["hooks"][0];
        assert_eq!(h["type"], "http");
        assert_eq!(
            h["url"],
            "https://127.0.0.1:7643/v1/agents/payments/backend/alice/events"
        );
        assert_eq!(h["headers"]["Authorization"], "Bearer s3");
        assert_eq!(
            hooks["Stop"], hooks["PreToolUse"],
            "user hooks are replaced"
        );
    }

    #[test]
    fn hosts_yml_and_claude_json_render() {
        assert_eq!(
            render_hosts_yml("gho_x"),
            "github.com:\n    oauth_token: gho_x\n    git_protocol: https\n"
        );
        let v = render_claude_json(Some(
            &json!({ "oauthAccount": { "emailAddress": "a@b.c" }, "hasCompletedOnboarding": true }),
        ));
        assert_eq!(v["hasCompletedOnboarding"], true);
        assert_eq!(v["oauthAccount"]["emailAddress"], "a@b.c");
        assert_eq!(
            render_claude_json(None),
            json!({ "hasCompletedOnboarding": true })
        );
    }

    fn creds() -> CredentialBundle {
        CredentialBundle {
            claude_credentials: Some(json!({ "claudeAiOauth": { "accessToken": "sk-SECRET" } })),
            claude_account: Some(json!({ "oauthAccount": { "emailAddress": "a@b.c" } })),
            gh_token: Some("gho_SECRET".into()),
        }
    }

    #[test]
    fn writes_every_file_with_the_right_mode() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StateLayout::from_env(dir.path(), |_| None);
        let paths = layout.agent(&id());
        let c = creds();
        write_home(
            &id(),
            &paths,
            &HomeInputs {
                settings: &json!({ "model": "opus" }),
                creds: &c,
                hooks: &hooks(),
                with_gh: true,
                redact_credentials: false,
            },
        )
        .unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&paths.claude_dir().join("settings.json")), 0o644);
        assert_eq!(mode(&paths.claude_dir().join(".credentials.json")), 0o600);
        assert_eq!(mode(&paths.gh_dir().join("hosts.yml")), 0o600);
        assert!(
            std::fs::read_to_string(paths.claude_dir().join(".credentials.json"))
                .unwrap()
                .contains("sk-SECRET")
        );
        assert!(
            std::fs::read_to_string(paths.gh_dir().join("hosts.yml"))
                .unwrap()
                .contains("gho_SECRET")
        );
        assert!(paths.nono_home.is_dir());
        assert!(paths.logs.is_dir());
    }

    #[test]
    fn redaction_replaces_secrets_and_no_gh_means_no_hosts_file() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StateLayout::from_env(dir.path(), |_| None);
        let paths = layout.agent(&id());
        let c = creds();
        write_home(
            &id(),
            &paths,
            &HomeInputs {
                settings: &json!({}),
                creds: &c,
                hooks: &hooks(),
                with_gh: false,
                redact_credentials: true,
            },
        )
        .unwrap();
        let creds_file =
            std::fs::read_to_string(paths.claude_dir().join(".credentials.json")).unwrap();
        assert!(!creds_file.contains("SECRET"));
        assert!(creds_file.contains(REDACTED));
        assert!(!paths.gh_dir().join("hosts.yml").exists());
    }
}
