//! Command-line surface. Every subcommand is a thin wrapper (spec §3):
//! parse args, call a library, print.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "hecaton",
    version,
    about = "Control plane and orchestrator for fleets of coding agents"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Inspect fleet configuration without talking to the daemon.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Developer tools; not part of the supported surface.
    #[command(hide = true)]
    Dev {
        #[command(subcommand)]
        command: DevCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the fully-resolved spec for a fleet file.
    Resolve(ResolveArgs),
}

#[derive(Debug, Args)]
pub struct ResolveArgs {
    /// Path to the fleet YAML file.
    pub file: PathBuf,
    /// Fleet name; overrides `name` in the file.
    #[arg(long)]
    pub name: Option<String>,
    /// Do not layer the host's ~/.claude/settings.json beneath the file.
    #[arg(long)]
    pub no_host_defaults: bool,
    /// Print JSON instead of YAML.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum DevCommand {
    /// Render one agent's generated files (settings.json, mise.toml,
    /// nono-profile.json, launch.sh) without launching anything.
    Materialize(MaterializeArgs),
}

#[derive(Debug, Args)]
pub struct MaterializeArgs {
    /// Path to the fleet YAML file.
    pub file: PathBuf,
    /// Agent to render, as crew/agent.
    pub agent: String,
    /// Output root (default: a fresh temp dir, printed).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Fleet name; overrides `name` in the file.
    #[arg(long)]
    pub name: Option<String>,
    /// Do not layer the host's ~/.claude/settings.json beneath the file.
    #[arg(long)]
    pub no_host_defaults: bool,
    /// Where the generated hooks post to.
    #[arg(long, default_value = "http://127.0.0.1:7643")]
    pub hooks_url: String,
    /// Also run `mise install` and `nono profile validate`.
    #[arg(long)]
    pub install: bool,
    /// Write real credentials instead of "<redacted>" placeholders.
    #[arg(long)]
    pub with_credentials: bool,
}
