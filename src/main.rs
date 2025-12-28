mod config;
mod model;
mod pool;
mod scanner;
mod server;
mod state;
mod web;

use config::AppConfig;
use pool::maintain_pool;
use scanner::start_scan_task;
use server::start_server;
use state::IpManager;
use web::start_web_server;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::info;

#[tokio::main]
async fn main() {
    // Initialize logging
    tracing_subscriber::fmt::init();

    info!("Starting TCP Forwarder...");

    // Load configuration (using default for now)
    let config = Arc::new(AppConfig::default());
    info!("Loaded configuration: {:?}", config);

    // Initialize IP Manager
    let ip_manager = IpManager::new();
    ip_manager.start_cleanup_task();

    // Initialize Warm Connection Pool
    let (pool_tx, pool_rx) = mpsc::channel(config.max_warm_connections);
    let pool_rx = Arc::new(Mutex::new(pool_rx));
    
    // Start pool maintainer
    let maintainer_manager = ip_manager.clone();
    let maintainer_config = config.clone();
    tokio::spawn(async move {
        maintain_pool(maintainer_manager, pool_tx, maintainer_config).await;
    });

    // Start TCP Forwarder Server
    let server_config = config.clone();
    let server_pool_rx = pool_rx.clone();
    let server_manager = ip_manager.clone();
    tokio::spawn(async move {
        start_server(server_config, server_pool_rx, server_manager).await;
    });

    // Start IP Scanner Task
    let scanner_config = config.clone();
    let scanner_manager = ip_manager.clone();
    tokio::spawn(async move {
        start_scan_task(scanner_config, scanner_manager).await;
    });

    // Start Web Server
    let web_manager = ip_manager.clone();
    tokio::spawn(async move {
        start_web_server(web_manager).await;
    });
    
    // Keep main thread alive
    tokio::signal::ctrl_c().await.unwrap();
    info!("Shutting down...");
}
