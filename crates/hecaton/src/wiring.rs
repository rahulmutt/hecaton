//! The only place adapters meet the process environment.

use anyhow::{Context, Result};
use hecaton_runtime::{StateLayout, ToolPaths};

pub fn layout_from_env() -> Result<StateLayout> {
    let home = std::env::home_dir().context("cannot determine the home directory")?;
    Ok(StateLayout::from_env(&home, |k| std::env::var_os(k)))
}

pub fn tool_paths() -> Result<ToolPaths> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let me = std::env::current_exe().context("cannot determine hecaton's own path")?;
    ToolPaths::discover_in(&path, &me).map_err(|e| {
        anyhow::anyhow!("{e} (hecaton needs git, gh, mise, nono and tmux on PATH; see mise.toml)")
    })
}
