mod config;
mod model;
mod scanner;
mod server;
mod state;
mod web;
use std::sync::Arc;

use config::AppConfig;
use scanner::run_scan_once;
use server::start_server;
use state::IpManager;
use web::start_web_server;
use tokio::sync::Semaphore;
use tracing::{info, warn};

#[tokio::main]
async fn main() {
    // Initialize logging
    tracing_subscriber::fmt::init();

    info!("Starting TCP Forwarder...");

    // Load configuration
    let config = match AppConfig::load_from_file("config.json") {
        Ok(cfg) => {
            info!("Loaded configuration from config.json");
            Arc::new(cfg)
        }
        Err(e) => {
            info!("Failed to load config.json: {}, using default configuration", e);
            Arc::new(AppConfig::default())
        }
    };
    info!("Loaded configuration: {:?}", config);

    // Initialize IP Manager
    let ip_manager = IpManager::new();

    // Check for --scan argument
    let args: Vec<String> = std::env::args().collect();
    if args.contains(&"--scan".to_string()) {
        info!("Running in scan mode...");
        let semaphore = Arc::new(Semaphore::new(200)); // Default max concurrency
        run_scan_once(config.clone(), ip_manager.clone(), 50, semaphore).await;
        
        if let Err(e) = ip_manager.save_to_file(&config.ip_store_file) {
            warn!("Failed to save scan results: {}", e);
        } else {
            info!("Scan results saved to {}", config.ip_store_file);
        }
        return;
    }

    // Normal mode: Load IPs from file
    if let Err(e) = ip_manager.load_from_file(&config.ip_store_file, config.selection_top_k_percent) {
        warn!("Failed to load IPs from file: {}. Starting with empty list.", e);
    } else {
        info!("Loaded IPs from {}", config.ip_store_file);
    }

    // Check for --rank-colos argument
    if args.contains(&"--rank-colos".to_string()) {
        print_colo_ranking(&ip_manager);
        return;
    }

    // Only start cleanup task if we are NOT using static file mode (or maybe we want to keep it but with longer timeout?)
    // For now, as per requirement "no need to scan often", we disable cleanup to prevent loaded IPs from expiring
    // ip_manager.start_cleanup_task();

    // Start TCP Forwarder Server (using concurrent connections instead of warm pool)
    let server_config = config.clone();
    let server_manager = ip_manager.clone();
    tokio::spawn(async move {
        start_server(server_config, server_manager).await;
    });

    // Start IP Scanner Task - DISABLED in normal mode as per requirement
    // let scanner_config = config.clone();
    // let scanner_manager = ip_manager.clone();
    // tokio::spawn(async move {
    //     start_scan_task(scanner_config, scanner_manager).await;
    // });

    // Start Web Server
    let web_manager = ip_manager.clone();
    let web_config = config.clone();
    tokio::spawn(async move {
        start_web_server(web_config, web_manager).await;
    });
    
    // Keep main thread alive
    tokio::signal::ctrl_c().await.unwrap();
    info!("Shutting down...");
}

fn print_colo_ranking(ip_manager: &IpManager) {
    let subnets = ip_manager.get_all_subnets();
    if subnets.is_empty() {
        println!("No subnet data available. Please run with --scan first.");
        return;
    }

    use std::collections::HashMap;

    struct ColoStats {
        total_score: f32,
        total_latency: u128,
        count: usize,
    }

    let mut stats_map: HashMap<String, ColoStats> = HashMap::new();

    for subnet in &subnets {
        let entry = stats_map.entry(subnet.colo.clone()).or_insert(ColoStats {
            total_score: 0.0,
            total_latency: 0,
            count: 0,
        });
        entry.total_score += subnet.score;
        entry.total_latency += subnet.avg_latency;
        entry.count += 1;
    }

    let mut ranking: Vec<(String, f32, u128, usize)> = stats_map
        .into_iter()
        .map(|(colo, stats)| {
            (
                colo,
                stats.total_score / stats.count as f32,
                if stats.count > 0 { stats.total_latency / stats.count as u128 } else { 0 },
                stats.count,
            )
        })
        .collect();

    // Sort by average score descending
    ranking.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    println!("{:<10} | {:<10} | {:<10} | {:<10}", "Colo", "Avg Score", "Avg Latency", "Subnet Count");
    println!("{:-<10}-|-{:-<10}-|-{:-<10}-|-{:-<10}", "", "", "", "");

    for (colo, avg_score, avg_latency, count) in ranking {
        println!("{:<10} | {:<10.2} | {:<10} | {:<10}", colo, avg_score, avg_latency, count);
    }
}
