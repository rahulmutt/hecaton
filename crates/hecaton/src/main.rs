//! `hecaton` — CLI entry point.

mod cli;
mod commands;
mod wiring;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command, ConfigCommand, DevCommand};

fn main() -> ExitCode {
    match run() {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<String> {
    let cli = Cli::parse();
    match cli.command {
        Command::Config {
            command: ConfigCommand::Resolve(args),
        } => commands::config::resolve_command(&args),
        Command::Dev {
            command: DevCommand::Materialize(args),
        } => commands::dev::materialize_command(&args),
    }
}
