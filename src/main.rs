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

/// 命令行参数结构体
#[derive(Debug, Default)]
struct CliArgs {
    scan: bool,
    rank_colos: bool,
    help: bool,
}

/// 解析命令行参数
///
/// # 返回值
/// 解析后的命令行参数结构体
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

/// 初始化日志系统
fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();
}

/// 加载应用配置
///
/// # 返回值
/// Arc 包装的应用配置
fn load_config() -> Arc<AppConfig> {
    match AppConfig::load_from_file("config.json") {
        Ok(cfg) => {
            info!("Loaded configuration from config.json");
            Arc::new(cfg)
        }
        Err(e) => {
            warn!(
                "Failed to load config.json: {}. \
                 Using default configuration.",
                e
            );
            warn!(
                "Note: Default configuration may not be optimal \
                 for your environment."
            );
            Arc::new(AppConfig::default())
        }
    }
}

/// 运行扫描模式
///
/// # 参数
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
async fn run_scan_mode(config: Arc<AppConfig>, ip_manager: IpManager) {
    info!("Running in scan mode...");
    run_scan_once(config.clone(), ip_manager.clone(), 200).await;

    match ip_manager.save_to_file(&config.ip_store_file) {
        Ok(_) => info!("Scan results saved to {}", config.ip_store_file),
        Err(e) => error!("Failed to save scan results: {}", e),
    }
}

/// 等待关闭信号并优雅停机
///
/// # 参数
/// - `server_handle`: 服务器任务句柄
/// - `web_handle`: Web 服务任务句柄
async fn graceful_shutdown(
    server_handle: tokio::task::JoinHandle<()>,
    web_handle: tokio::task::JoinHandle<()>,
) {
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
}

/// 启动转发服务并等待关闭信号
///
/// # 参数
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
async fn run_forward_mode(config: Arc<AppConfig>, ip_manager: IpManager) {
    // 创建取消令牌用于优雅关闭
    let cancel_token = CancellationToken::new();

    // 启动 TCP 转发服务器
    let server_handle = tokio::spawn(start_server(
        config.clone(),
        ip_manager.clone(),
        cancel_token.clone(),
    ));

    // 启动 Web 服务器
    let web_handle = tokio::spawn(start_web_server(
        config.clone(),
        ip_manager.clone(),
        cancel_token.clone(),
    ));

    // 等待关闭信号
    let _ = tokio::signal::ctrl_c().await;
    info!("Received Ctrl+C, initiating graceful shutdown...");

    // 发送取消信号
    cancel_token.cancel();

    graceful_shutdown(server_handle, web_handle).await;
}

#[tokio::main]
async fn main() {
    init_logging();

    let cli_args = parse_args();
    if cli_args.help {
        print_help();
        return;
    }

    info!("Starting TCP Forwarder...");
    let config = load_config();
    info!("Loaded configuration: {:?}", config);

    let ip_manager = IpManager::new();

    if cli_args.scan {
        run_scan_mode(config, ip_manager).await;
        return;
    }

    // 正常模式：从文件加载 IP
    match ip_manager.load_from_file(
        &config.ip_store_file,
        config.selection_top_k_percent,
    ) {
        Ok(_) => info!("Loaded IPs from {}", config.ip_store_file),
        Err(e) => warn!(
            "Failed to load IPs from file: {}. \
             Starting with empty list.",
            e
        ),
    }

    if cli_args.rank_colos {
        analytics::print_colo_ranking(&ip_manager);
        return;
    }

    run_forward_mode(config, ip_manager).await;
    info!("TCP Forwarder stopped");
}
