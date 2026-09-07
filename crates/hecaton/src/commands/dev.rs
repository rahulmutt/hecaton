//! `hecaton dev …` (Phase 2 spec §5; `fake-claude` is Phase 3 spec §7).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hecaton_config::{HostPaths, ResolveOptions, host, read, resolve};
use hecaton_core::{Fleet, HookTarget, ResolvedAgent};
use hecaton_runtime::{RenderOptions, Runtime, StateLayout};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::cli::{FakeClaudeArgs, MaterializeArgs};
use crate::wiring::{layout_from_env, tool_paths};

pub fn materialize_command(args: &MaterializeArgs) -> Result<String> {
    if args.agent.split('/').count() != 2 {
        bail!("agent must be crew/agent");
    }
    let file = read(&args.file)?;
    let defaults = if args.no_host_defaults {
        host::HostDefaults::default()
    } else {
        host::load(&HostPaths::discover()?)?
    };
    let spec = resolve(
        &file,
        &ResolveOptions {
            name_override: args.name.clone(),
            host_claude_settings: defaults.claude_settings,
        },
    )?;
    let fleet = Fleet::try_from(spec)?;
    let wanted = format!("{}/{}", fleet.name, args.agent);
    let agent = ResolvedAgent::from_fleet(&fleet)
        .into_iter()
        .find(|a| a.id.to_string() == wanted)
        .with_context(|| format!("no agent {wanted:?} in the fleet (expected crew/agent)"))?;

    let real = layout_from_env()?;
    let tools = tool_paths()?;

    // Only claim (`.keep()`) the fallback temp dir after everything below
    // succeeds; on an early return via `?` this drops and cleans up.
    let mut fallback_tmp = None;
    let out_root = match &args.out {
        Some(p) => p.clone(),
        None => {
            let dir = tempfile::Builder::new()
                .prefix("hecaton-materialize-")
                .tempdir()?;
            let path = dir.path().to_path_buf();
            fallback_tmp = Some(dir);
            path
        }
    };
    let layout = StateLayout {
        state_root: out_root.join("state"),
        data_root: out_root.join("data"),
        config_root: real.config_root,
    };
    let rt = Runtime::new(layout.clone(), tools);

    let paths = layout.agent(&agent.id);
    std::fs::create_dir_all(&paths.workspace)?;
    std::fs::create_dir_all(layout.crew(&agent.id.crew_ref()).repo.join(".git"))?;
    let hooks = HookTarget {
        url: args.hooks_url.clone(),
        secret: throwaway_secret(),
    };
    rt.render_agent(
        &agent,
        &defaults.credentials,
        &hooks,
        &RenderOptions {
            redact_credentials: !args.with_credentials,
        },
    )?;
    if args.install {
        rt.install_and_validate(&agent)?;
    }
    if let Some(dir) = fallback_tmp {
        let _ = dir.keep();
    }

    let mut out = format!("agent dir: {}\n", paths.root.display());
    let credentials_path = paths.claude_dir().join(".credentials.json");
    let hosts_path = paths.gh_dir().join("hosts.yml");
    for (label, p) in [
        ("settings.json", paths.claude_dir().join("settings.json")),
        (".credentials.json", credentials_path.clone()),
        ("hosts.yml", hosts_path.clone()),
        ("mise.toml", paths.mise_toml.clone()),
        ("nono-profile.json", paths.profile.clone()),
        ("launch.sh", paths.launch.clone()),
    ] {
        if p.exists() {
            out.push_str(&format!("  {label:<19} {}\n", p.display()));
        }
    }
    out.push_str(if args.with_credentials {
        "credentials: written (real)\n"
    } else if credentials_path.exists() || hosts_path.exists() {
        "credentials: <redacted> placeholders (pass --with-credentials to write them)\n"
    } else {
        "credentials: none found on the host (nothing to redact)\n"
    });
    if !args.install {
        out.push_str("not run: mise install, nono profile validate (pass --install)\n");
    }
    Ok(out)
}

/// Not a real secret: only so the rendered settings.json has the right shape.
fn throwaway_secret() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hex::encode(Sha256::digest(format!("{}-{nanos}", std::process::id())))[..32].to_string()
}

/// `hecaton dev fake-claude` (Phase 3 spec §7): what the e2e launches in
/// place of `claude`. Runs once, then sleeps until the runner kills it.
pub fn fake_claude_command(args: &FakeClaudeArgs) -> Result<String> {
    let config_dir =
        PathBuf::from(std::env::var_os("CLAUDE_CONFIG_DIR").context("CLAUDE_CONFIG_DIR not set")?);
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME not set")?);
    fake_claude_once(&config_dir, &home, &args.rest)?;
    eprintln!("fake-claude: hooks done; sleeping until killed");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Records argv, marks a session so `--continue` triggers next time, runs
/// every `SessionStart` command hook with a payload on stdin (as Claude
/// does), and posts one `Notification` to every HTTP hook for that event.
pub fn fake_claude_once(config_dir: &Path, home: &Path, argv: &[String]) -> Result<()> {
    std::fs::write(home.join("fake-claude.argv"), argv.join("\n") + "\n")?;
    let projects = config_dir.join("projects").join("e2e");
    std::fs::create_dir_all(&projects)?;
    std::fs::write(projects.join("session.marker"), "fake\n")?;
    let settings: Value =
        serde_json::from_slice(&std::fs::read(config_dir.join("settings.json"))?)?;
    let cwd = std::env::current_dir()?;

    for hook in hooks_of(&settings, "SessionStart") {
        let Some(cmd) = (hook["type"] == "command")
            .then(|| hook["command"].as_str())
            .flatten()
        else {
            continue;
        };
        let payload = json!({
            "hook_event_name": "SessionStart", "session_id": "fake-claude",
            "cwd": cwd, "source": "startup"
        });
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .env("HOME", home)
            .env("CLAUDE_CONFIG_DIR", config_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("cannot run SessionStart hook {cmd:?}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(payload.to_string().as_bytes())?;
        }
        let out = child.wait_with_output()?;
        eprintln!(
            "fake-claude: SessionStart hook {cmd:?} exited {} with {}",
            out.status,
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }

    for hook in hooks_of(&settings, "Notification") {
        let Some(url) = (hook["type"] == "http")
            .then(|| hook["url"].as_str())
            .flatten()
        else {
            continue;
        };
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .http_status_as_error(false)
            .build()
            .into();
        let mut req = agent.post(url).header("Content-Type", "application/json");
        if let Some(headers) = hook["headers"].as_object() {
            for (k, v) in headers {
                if let Some(v) = v.as_str() {
                    req = req.header(k, v);
                }
            }
        }
        let payload = json!({
            "hook_event_name": "Notification", "session_id": "fake-claude",
            "message": "fake claude is up"
        });
        // A raw compact body, not `send_json` (which pretty-prints), to
        // match the payload shape Claude actually sends.
        match req.send(payload.to_string().as_bytes()) {
            Ok(r) => eprintln!("fake-claude: Notification hook → {}", r.status()),
            Err(e) => eprintln!("fake-claude: Notification hook failed: {e}"),
        }
    }
    Ok(())
}

fn hooks_of(settings: &Value, event: &str) -> Vec<Value> {
    settings["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["hooks"].as_array().cloned().unwrap_or_default())
        .collect()
}

/// `hecaton dev fake-plugin`: the e2e's plugin. Binds a loopback listener
/// (plugins spec §11.1 row 1: can a sandboxed plugin bind at all?), says
/// hello with that address, records the reply, sleeps until killed. If the
/// bind is refused it still says hello with `127.0.0.1:0` and leaves
/// `fake-plugin.bind-failed` in scratch, so the e2e reports the verdict
/// instead of hanging.
pub fn fake_plugin_command() -> Result<String> {
    use hecaton_plugin_sdk::{Env, Host};
    let env = Env::from_process()?;
    let scratch = env.scratch.clone();
    std::fs::create_dir_all(&scratch)?;
    let (listener, listen) = match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(l) => {
            let addr = l.local_addr()?.to_string();
            (Some(l), addr)
        }
        Err(e) => {
            eprintln!("fake-plugin: cannot bind a loopback listener: {e}");
            std::fs::write(scratch.join("fake-plugin.bind-failed"), e.to_string())?;
            (None, "127.0.0.1:0".to_string())
        }
    };
    let host = Host::new(env)?;
    let rt = tokio::runtime::Runtime::new()?;
    let resp = rt.block_on(host.hello(env!("CARGO_PKG_VERSION"), &listen))?;
    std::fs::write(
        scratch.join("fake-plugin.hello"),
        serde_json::to_string_pretty(&resp.config)?,
    )?;
    eprintln!("fake-plugin: hello acknowledged; listening on {listen}; sleeping until killed");
    let _keep = listener;
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::stub_server;
    use serde_json::json;

    #[test]
    fn fake_claude_runs_command_hooks_posts_one_http_hook_and_records_argv() {
        let (url, seen) = stub_server("200 OK", "{}");
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let config_dir = home.join(".claude");
        std::fs::create_dir_all(&config_dir).unwrap();
        let settings = json!({
            "hooks": {
                "SessionStart": [{ "hooks": [{ "type": "command", "command": "cat > \"$HOME/seen.json\"", "timeout": 10 }] }],
                "Notification": [{ "hooks": [{ "type": "http", "url": format!("{url}/v1/agents/f/c/a/events"), "headers": { "Authorization": "Bearer s3" } }] }],
                "Stop": [{ "hooks": [{ "type": "http", "url": "http://127.0.0.1:1/never" }] }]
            }
        });
        std::fs::write(config_dir.join("settings.json"), settings.to_string()).unwrap();
        fake_claude_once(
            &config_dir,
            &home,
            &["--verbose".into(), "--continue".into()],
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(home.join("fake-claude.argv")).unwrap(),
            "--verbose\n--continue\n"
        );
        let seen_payload: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join("seen.json")).unwrap())
                .unwrap();
        assert_eq!(seen_payload["hook_event_name"], "SessionStart");
        assert!(config_dir.join("projects/e2e/session.marker").exists());
        let req = seen.recv().unwrap();
        assert!(
            req.contains("Authorization: Bearer s3") || req.contains("authorization: Bearer s3"),
            "{req}"
        );
        assert!(req.contains(r#""hook_event_name":"Notification""#), "{req}");
    }
}
