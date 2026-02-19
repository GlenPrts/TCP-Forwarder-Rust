mod analytics;
mod config;
mod model;
mod pool;
mod scanner;
mod server;
mod state;
mod utils;
mod web;

use std::sync::Arc;

use config::AppConfig;
use pool::{run_refill_loop, ConnectionPool};
use scanner::{run_background_scan, run_scan_once};
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
    --scan            运行扫描模式，扫描优选 IP 段
    --scan-on-start   转发模式启动时立即执行一次扫描
    --rank-colos      显示数据中心质量排名
    --help, -h        显示此帮助信息

说明:
    后台定时扫描通过 config.jsonc 中的
    background_scan 配置块控制。
    --scan-on-start 仅在转发模式下生效，
    启动时立即执行一次扫描后再进入定时循环。

示例:
    tcp-forwarder-rust --scan            # 扫描优选 IP
    tcp-forwarder-rust --rank-colos      # 查看排名
    tcp-forwarder-rust                   # 启动转发
    tcp-forwarder-rust --scan-on-start   # 转发+立即扫描
"#
    );
}

/// 命令行参数结构体
#[derive(Debug, Default)]
struct CliArgs {
    scan: bool,
    scan_on_start: bool,
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
            "--scan-on-start" => cli_args.scan_on_start = true,
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
    match AppConfig::load_from_file("config.jsonc") {
        Ok(cfg) => {
            info!("Loaded configuration from config.jsonc");
            Arc::new(cfg)
        }
        Err(e) => {
            warn!(
                "Failed to load config.jsonc: {}. \
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

/// 创建预连接池（如果配置启用）
///
/// # 参数
/// - `config`: 应用配置
///
/// # 返回值
/// 连接池实例（如果启用）
fn create_connection_pool(config: &Arc<AppConfig>) -> Option<Arc<ConnectionPool>> {
    if !config.connection_pool.enabled {
        return None;
    }
    info!(
        "Connection pool enabled (size={}, idle={}s)",
        config.connection_pool.pool_size, config.connection_pool.max_idle_secs,
    );
    Some(Arc::new(ConnectionPool::new(
        config.connection_pool.clone(),
    )))
}

/// 启动连接池后台补充任务
///
/// # 参数
/// - `pool`: 连接池
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
/// - `cancel_token`: 取消令牌
///
/// # 返回值
/// 后台任务句柄（如果启用）
fn spawn_pool_refill(
    pool: &Option<Arc<ConnectionPool>>,
    config: &Arc<AppConfig>,
    ip_manager: &IpManager,
    cancel_token: &CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let pool = pool.as_ref()?.clone();
    Some(tokio::spawn(run_refill_loop(
        pool,
        config.clone(),
        ip_manager.clone(),
        cancel_token.clone(),
    )))
}

/// 等待关闭信号并优雅停机
///
/// # 参数
/// - `server_handle`: 服务器任务句柄
/// - `web_handle`: Web 服务任务句柄
/// - `scan_handle`: 后台扫描任务句柄（可选）
/// - `pool_handle`: 连接池补充任务句柄（可选）
async fn graceful_shutdown(
    server_handle: tokio::task::JoinHandle<()>,
    web_handle: tokio::task::JoinHandle<()>,
    scan_handle: Option<tokio::task::JoinHandle<()>>,
    pool_handle: Option<tokio::task::JoinHandle<()>>,
) {
    let shutdown_timeout = tokio::time::Duration::from_secs(10);
    tokio::select! {
        _ = async {
            let _ = server_handle.await;
            let _ = web_handle.await;
            if let Some(h) = scan_handle {
                let _ = h.await;
            }
            if let Some(h) = pool_handle {
                let _ = h.await;
            }
        } => {
            info!("All services shut down gracefully");
        }
        _ = tokio::time::sleep(shutdown_timeout) => {
            warn!(
                "Shutdown timeout reached, forcing exit"
            );
        }
    }
}

/// 启动转发服务并等待关闭信号
///
/// # 参数
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
/// - `scan_on_start`: 是否启动时立即扫描
async fn run_forward_mode(config: Arc<AppConfig>, ip_manager: IpManager, scan_on_start: bool) {
    let cancel_token = CancellationToken::new();

    let pool = create_connection_pool(&config);

    let pool_handle = spawn_pool_refill(&pool, &config, &ip_manager, &cancel_token);

    let server_handle = tokio::spawn(start_server(
        config.clone(),
        ip_manager.clone(),
        pool.clone(),
        cancel_token.clone(),
    ));

    let web_handle = tokio::spawn(start_web_server(
        config.clone(),
        ip_manager.clone(),
        pool.clone(),
        cancel_token.clone(),
    ));

    let scan_handle = spawn_background_scan(&config, &ip_manager, &cancel_token, scan_on_start);

    let _ = tokio::signal::ctrl_c().await;
    info!("Received Ctrl+C, initiating graceful shutdown...");

    cancel_token.cancel();

    graceful_shutdown(server_handle, web_handle, scan_handle, pool_handle).await;
}

/// 启动后台扫描任务（如果配置启用）
///
/// # 参数
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
/// - `cancel_token`: 取消令牌
/// - `scan_on_start`: 是否启动时立即扫描
///
/// # 返回值
/// 后台任务句柄（如果启用）
fn spawn_background_scan(
    config: &Arc<AppConfig>,
    ip_manager: &IpManager,
    cancel_token: &CancellationToken,
    scan_on_start: bool,
) -> Option<tokio::task::JoinHandle<()>> {
    // 允许 CLI 参数覆盖配置文件
    if !config.background_scan.enabled && !scan_on_start {
        return None;
    }

    if scan_on_start && !config.background_scan.enabled {
        info!("CLI --scan-on-start forced background scan ON");
    }

    Some(tokio::spawn(run_background_scan(
        config.clone(),
        ip_manager.clone(),
        cancel_token.clone(),
        scan_on_start,
    )))
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
    ip_manager.set_max_open_files(config.max_open_files);

    if cli_args.scan {
        run_scan_mode(config, ip_manager).await;
        return;
    }

    // 正常模式：从文件加载 IP
    match ip_manager.load_from_file(&config.ip_store_file, config.selection_top_k_percent) {
        Ok(_) => info!("Loaded IPs from {}", config.ip_store_file),
        Err(e) => warn!(
            "Failed to load IPs from file: {}. \
             Starting with empty list.",
            e
        ),
    }

    if cli_args.rank_colos {
        let manager = ip_manager.clone();
        tokio::task::spawn_blocking(move || {
            analytics::print_colo_ranking(&manager);
        })
        .await
        .unwrap_or(());
        return;
    }

    run_forward_mode(config, ip_manager, cli_args.scan_on_start).await;
    info!("TCP Forwarder stopped");
}
