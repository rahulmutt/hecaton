//! `hecaton dev …` (Phase 2 spec §5).

use anyhow::{Context, Result, bail};
use hecaton_config::{HostPaths, ResolveOptions, host, read, resolve};
use hecaton_core::{Fleet, HookTarget, ResolvedAgent};
use hecaton_runtime::{RenderOptions, Runtime, StateLayout};
use sha2::{Digest, Sha256};

use crate::cli::MaterializeArgs;
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
