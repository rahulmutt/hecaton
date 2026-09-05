//! `hecaton` — CLI entry point.

mod cli;
mod commands;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command, ConfigCommand};

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
    }
}
