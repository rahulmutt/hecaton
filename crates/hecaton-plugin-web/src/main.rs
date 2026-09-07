//! `hecaton-plugin-web`: read the daemon's environment, start the watch,
//! say hello, serve. Failures print `web: …` to stderr and exit 1; that
//! lands in the plugin's tmux window and `plugins/web/logs/`.

use hecaton_plugin_sdk::{Env, Host, serve};
use hecaton_plugin_web::WebPlugin;

fn run() -> anyhow::Result<()> {
    let env = Env::from_process()?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let host = Host::new(env.clone())?;
        let plugin = WebPlugin::new(Host::new(env)?)?;
        let watch = plugin.start_watch();
        eprintln!("web: starting");
        let result = serve(&host, env!("CARGO_PKG_VERSION"), plugin).await;
        watch.abort();
        result?;
        Ok(())
    })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("web: {e:#}");
        std::process::exit(1);
    }
}
