use serde::Deserialize;
use tracing_subscriber::{fmt, EnvFilter};
use tracing_subscriber::prelude::*; // for .with()

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub listenAddr: String,
}

pub fn init_tracing() {
    let fmt_layer = fmt::layer().with_target(false);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();
}
