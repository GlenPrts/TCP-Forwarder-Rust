mod analytics;
mod config;
mod model;
mod scanner;
mod server;
mod state;
mod utils;
mod web;

use std::sync::Arc;

use config::AppConfig;
use scanner::run_scan_once;
use server::start_server;
use state::IpManager;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use web::start_web_server;

/// 打印帮助信息
fn print_help() {
    println!(
        r#"TCP Forwarder Rust - 高性能 TCP 转发器

用法:
    tcp-forwarder-rust [选项]

选项:
    --scan          运行扫描模式，扫描优选 IP 段
    --rank-colos    显示数据中心质量排名
    --help, -h      显示此帮助信息

示例:
    tcp-forwarder-rust --scan       # 扫描优选 IP
    tcp-forwarder-rust --rank-colos # 查看数据中心排名
    tcp-forwarder-rust              # 启动转发服务
"#
    );
}

/// 解析命令行参数
#[derive(Debug, Default)]
struct CliArgs {
    scan: bool,
    rank_colos: bool,
    help: bool,
}

fn parse_args() -> CliArgs {
    let args: Vec<String> = std::env::args().collect();
    let mut cli_args = CliArgs::default();

    for arg in args.iter().skip(1) {
        match arg.as_str() {
            "--scan" => cli_args.scan = true,
            "--rank-colos" => cli_args.rank_colos = true,
            "--help" | "-h" => cli_args.help = true,
            _ => {
                eprintln!("未知参数: {}", arg);
                cli_args.help = true;
            }
        }
    }

    cli_args
}

#[tokio::main]
async fn main() {
    // 初始化日志
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let cli_args = parse_args();

    if cli_args.help {
        print_help();
        return;
    }

    info!("Starting TCP Forwarder...");

    // 加载配置
    let config = match AppConfig::load_from_file("config.json") {
        Ok(cfg) => {
            info!("Loaded configuration from config.json");
            Arc::new(cfg)
        }
        Err(e) => {
            error!(
                "Failed to load config.json: {}. Using default configuration may cause unexpected behavior.",
                e
            );
            Arc::new(AppConfig::default())
        }
    };
    info!("Loaded configuration: {:?}", config);

    // 初始化 IP 管理器
    let ip_manager = IpManager::new();

    // 扫描模式
    if cli_args.scan {
        info!("Running in scan mode...");
        let semaphore = Arc::new(Semaphore::new(200));
        run_scan_once(config.clone(), ip_manager.clone(), semaphore).await;

        if let Err(e) = ip_manager.save_to_file(&config.ip_store_file) {
            error!("Failed to save scan results: {}", e);
        } else {
            info!("Scan results saved to {}", config.ip_store_file);
        }
        return;
    }

    // 正常模式：从文件加载 IP
    if let Err(e) = ip_manager.load_from_file(&config.ip_store_file, config.selection_top_k_percent)
    {
        warn!(
            "Failed to load IPs from file: {}. Starting with empty list.",
            e
        );
    } else {
        info!("Loaded IPs from {}", config.ip_store_file);
    }

    // 数据中心排名模式
    if cli_args.rank_colos {
        analytics::print_colo_ranking(&ip_manager);
        return;
    }

    // 创建取消令牌用于优雅关闭
    let cancel_token = CancellationToken::new();

    // 启动 TCP 转发服务器
    let server_config = config.clone();
    let server_manager = ip_manager.clone();
    let server_cancel = cancel_token.clone();
    let server_handle = tokio::spawn(async move {
        start_server(server_config, server_manager, server_cancel).await;
    });

    // 启动 Web 服务器
    let web_manager = ip_manager.clone();
    let web_config = config.clone();
    let web_cancel = cancel_token.clone();
    let web_handle = tokio::spawn(async move {
        start_web_server(web_config, web_manager, web_cancel).await;
    });

    // 等待关闭信号
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, initiating graceful shutdown...");
        }
    }

    // 发送取消信号
    cancel_token.cancel();

    // 等待所有任务完成（带超时）
    let shutdown_timeout = tokio::time::Duration::from_secs(10);
    tokio::select! {
        _ = async {
            let _ = server_handle.await;
            let _ = web_handle.await;
        } => {
            info!("All services shut down gracefully");
        }
        _ = tokio::time::sleep(shutdown_timeout) => {
            warn!("Shutdown timeout reached, forcing exit");
        }
    }

    info!("TCP Forwarder stopped");
}

