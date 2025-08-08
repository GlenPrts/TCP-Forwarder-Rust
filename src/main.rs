mod config;

use anyhow::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    config::init_tracing();
    info!("tcp-forwarder starting...");
    // TODO: load config, start components
    Ok(())
}
