//! `hecaton-plugin-matrix`: read the daemon's environment, build the
//! actor's plumbing, say hello, serve. Failures print `matrix: …` to
//! stderr and exit 1; that lands in the plugin's tmux window and
//! `plugins/matrix/logs/`. The actor itself does not start until
//! `configure` arrives with the homeserver and the credentials.

use hecaton_plugin_matrix::MatrixPlugin;
use hecaton_plugin_matrix::actor::{Counters, Health, Queue};
use hecaton_plugin_matrix::client::MatrixLauncher;
use hecaton_plugin_sdk::{Env, Host, Metrics, serve};

fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let env = Env::from_process()?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let host = Host::new(env.clone())?;
        let metrics = Metrics::new(&env.name);
        let counters = Counters::new(&metrics)?;
        let health = Health::new();
        let queue = Queue::new(counters.events_dropped.clone());
        let launcher = MatrixLauncher {
            host: Host::new(env.clone())?,
            counters: counters.clone(),
            health: health.clone(),
        };
        let plugin = MatrixPlugin::new(metrics, counters, health, queue, launcher);
        eprintln!("matrix: starting");
        serve(&host, env!("CARGO_PKG_VERSION"), plugin).await?;
        Ok(())
    })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("matrix: {e:#}");
        std::process::exit(1);
    }
}
