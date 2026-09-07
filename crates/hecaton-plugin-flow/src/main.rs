//! `hecaton-plugin-flow`: read the daemon's environment, say hello, serve.
//! Failures print `flow: …` to stderr and exit 1; that lands in the
//! plugin's tmux window and `plugins/flow/logs/`.

use hecaton_plugin_flow::FlowPlugin;
use hecaton_plugin_sdk::{Env, Host, serve};

fn run() -> anyhow::Result<()> {
    let env = Env::from_process()?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let host = Host::new(env.clone())?;
        let plugin = FlowPlugin::new(Host::new(env)?)?;
        eprintln!("flow: starting");
        serve(&host, env!("CARGO_PKG_VERSION"), plugin).await?;
        Ok(())
    })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("flow: {e:#}");
        std::process::exit(1);
    }
}
