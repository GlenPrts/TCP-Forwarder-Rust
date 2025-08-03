mod config;
mod scorer;
mod probing;
mod selector;
mod pools;
mod metrics;

use anyhow::{Context, Result};
use config::{Config};
use std::path::Path;
use std::net::{SocketAddr, IpAddr};
use std::sync::Arc;
use std::time::Duration;
// 删除未使用的导入
use std::fs::File;
use std::io::{BufRead, BufReader};
use rand::prelude::*;
use tokio::net::{TcpListener, TcpStream};
use tokio::io::copy_bidirectional;
use tokio::signal;
use tracing::{info, warn, error, debug, Level, instrument};
use tracing_subscriber::EnvFilter;
use selector::ActiveRemotes;

use crate::metrics::METRICS;

/// 主函数
#[tokio::main]
async fn main() -> Result<()> {
    // 加载配置文件
    let config = load_config("config.yaml")?;
    
    // 初始化日志系统
    init_logging(&config.logging)?;
    
    // 初始化指标系统
    if config.metrics.enabled {
        METRICS.initialize().await?;
        info!("指标系统已启动，监听地址: {}", config.metrics.listen_addr);
    }
    
    // 记录启动时间
    let start_time = std::time::Instant::now();
    
    // 记录启动日志
    info!(
        version = env!("CARGO_PKG_VERSION"),
        listen_addr = %config.server.listen_addr,
        "TCP转发服务已启动"
    );
    
    // 创建评分板
    let score_board = scorer::create_score_board();
    
    // 从配置文件加载IP列表
    load_ip_list(&score_board, &config.remotes)?;
    
    // 执行启动时的初始探测，快速获得所有IP的初始评分
    info!("开始初始化IP探测，为快速启动准备...");
    if let Err(e) = probing::initial_probing(score_board.clone(), &config.remotes).await {
        warn!("初始探测失败，但不影响后续运行: {}", e);
    } else {
        info!("初始探测完成，系统已准备好处理连接");
    }
    
    // 创建活跃远程地址列表
    let active_remotes = Arc::new(tokio::sync::RwLock::new(Vec::new()));
    
    // 创建连接池管理器
    let pool_manager = pools::create_pool_manager();

    // 启动选择器任务（在初始探测完成后，选择器会立即执行一次评估）
    let selector_score_board = score_board.clone();
    let selector_active_remotes = active_remotes.clone();
    let selector_config = config.remotes.selector.clone();
    tokio::spawn(async move {
        if let Err(e) = selector::selector_task(selector_score_board, selector_active_remotes, selector_config).await {
            error!("选择器任务错误: {}", e);
        }
    });

    // 启动周期性探测任务（用于后续的定期更新）
    let probing_score_board = score_board.clone();
    let probing_config = config.remotes.clone();
    tokio::spawn(async move {
        if let Err(e) = probing::probing_task(probing_score_board, probing_config).await {
            error!("探测任务错误: {}", e);
        }
    });
    
    // 启动连接池管理任务
    let pool_score_board = score_board.clone();
    let pool_active_remotes = active_remotes.clone();
    let pool_remotes_config = config.remotes.clone();
    let pool_pools_config = config.pools.clone();
    let pool_manager_clone = pool_manager.clone();
    tokio::spawn(async move {
        if let Err(e) = pools::pool_manager_task(pool_manager_clone, pool_active_remotes, pool_score_board, Arc::new(pool_remotes_config), Arc::new(pool_pools_config)).await {
            error!("连接池管理任务错误: {}", e);
        }
    });
    
    // 启动指标服务器（如果启用）
    if config.metrics.enabled {
        let metrics_listen_addr = config.metrics.listen_addr;
        let metrics_path = config.metrics.path.clone();
        tokio::spawn(async move {
            if let Err(e) = METRICS.start_server(metrics_listen_addr, metrics_path).await {
                error!("指标服务器错误: {}", e);
            }
        });
    }
    
    // 启动运行时间更新任务
    let start_time_clone = start_time;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            let uptime = start_time_clone.elapsed().as_secs_f64();
            METRICS.record_uptime(uptime);
        }
    });
    
    // 记录远程端口，用于转发连接
    let remote_port = config.remotes.default_remote_port;
    
    // 创建TCP监听器
    let listener = TcpListener::bind(config.server.listen_addr)
        .await
        .context("无法绑定到指定地址")?;

    info!("TCP服务器已启动，正在监听: {}", config.server.listen_addr);

    // 主循环：接受新的连接并处理
    loop {
        tokio::select! {
            // 监听Ctrl+C信号
            _ = signal::ctrl_c() => {
                info!("收到停机信号，开始优雅停机...");
                break;
            }
            // 接受新的连接
            accept_result = listener.accept() => {
                let (client_socket, client_addr) = match accept_result {
                    Ok((socket, addr)) => {
                        // 记录新连接
                        METRICS.record_new_connection();
                        (socket, addr)
                    },
                    Err(e) => {
                        error!("接受连接失败: {}", e);
                        METRICS.record_error();
                        continue;
                    }
                };
                
                info!("接受到来自 {} 的连接", client_addr);
                
                // 为每个连接创建新的异步任务处理
                let ar_clone = active_remotes.clone();
                let remote_port_clone = remote_port;
                let score_board_clone = score_board.clone();
                let pool_manager_clone = pool_manager.clone();
                
                tokio::spawn(async move {
                    let result = handle_connection(client_socket, client_addr, ar_clone, score_board_clone, pool_manager_clone, remote_port_clone).await;
                    
                    // 记录连接结束
                    METRICS.record_connection_closed();
                    
                    // 根据结果记录成功或失败
                    match result {
                        Ok(_) => {
                            debug!(client_addr = %client_addr, "连接正常结束");
                            METRICS.record_connection_success();
                        },
                        Err(e) => {
                            error!(client_addr = %client_addr, "处理连接时出错: {}", e);
                            METRICS.record_connection_failed();
                        }
                    }
                });
            }
        }
    }
    
    // 优雅停机：等待一段时间让现有连接完成
    info!("等待现有连接完成处理...");
    tokio::time::sleep(Duration::from_secs(5)).await;
    info!("TCP转发服务已停止");
    
    Ok(())
}

/// 加载配置文件
fn load_config(config_path: impl AsRef<Path>) -> Result<Config> {
    // 从配置文件中读取配置
    let settings = ::config::Config::builder()
        .add_source(::config::File::from(config_path.as_ref()))
        .build()
        .context("无法解析配置文件")?;
    
    // 将配置反序列化为Config结构体
    let config: Config = settings.try_deserialize()
        .context("无法将配置反序列化为Config结构体")?;
    
    Ok(config)
}

/// 加载IP列表
fn load_ip_list(score_board: &scorer::ScoreBoard, config: &config::RemotesConfig) -> Result<()> {
    // 目前只支持从文件加载
    if config.provider.provider_type != "file" || config.provider.file.is_none() {
        return Err(anyhow::anyhow!("只支持从文件加载IP列表"));
    }
    
    let file_config = config.provider.file.as_ref().unwrap();
    let file_path = &file_config.path;
    
    info!("从文件加载IP列表: {:?}", file_path);
    
    let file = File::open(file_path)
        .context(format!("无法打开IP列表文件: {:?}", file_path))?;
    
    let reader = BufReader::new(file);
    let mut count = 0;
    
    for line in reader.lines() {
        let line = line.context("读取IP列表行失败")?;
        let line = line.trim();
        
        // 跳过空行和注释
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        
        // 尝试解析IP地址
        let ip: IpAddr = match line.parse() {
            Ok(ip) => ip,
            Err(e) => {
                warn!("无法解析IP地址: {}, 错误: {}", line, e);
                continue;
            }
        };
        
        // 创建初始评分数据
        let score_data = scorer::ScoreData::new(
            ip,
            config.default_remote_port,
            &config.scoring,
        );
        
        // 添加到评分板
        score_board.insert(ip, score_data);
        count += 1;
    }
    
    info!("已加载 {} 个IP地址到评分系统", count);
    
    Ok(())
}

/// 处理单个客户端连接
#[instrument(skip(client_socket, active_remotes, score_board, pool_manager))]
async fn handle_connection(
    mut client_socket: TcpStream, 
    client_addr: SocketAddr,
    active_remotes: ActiveRemotes,
    score_board: scorer::ScoreBoard,
    pool_manager: pools::PoolManager,
    remote_port: u16
) -> Result<()> {
    // 从活跃IP列表中选择一个目标IP
    let selected_ip = match select_target_ip(&active_remotes).await {
        Ok(ip) => ip,
        Err(e) => {
            warn!("无法选择目标IP: {}", e);
            // 等待一下再重试，也许活跃IP列表会更新
            tokio::time::sleep(Duration::from_millis(500)).await;
            select_target_ip(&active_remotes).await?
        }
    };
    
    // 构建完整的远程地址（IP:端口）
    let remote_addr = format!("{}:{}", selected_ip, remote_port);
    
    // 记录开始处理连接
    debug!("开始处理来自 {} 的连接，目标地址: {}", client_addr, remote_addr);
    
    // 首先尝试从连接池获取预建立的连接
    let mut remote_socket = match pools::get_connection_from_pool(&pool_manager, selected_ip).await {
        Some(mut pooled_connection) => {
            // 从连接池获得连接，记录连接池命中
            debug!("从连接池获得到 {} 的连接", remote_addr);
            METRICS.record_pool_hit(&selected_ip.to_string());
            // 更新连接活跃时间
            pooled_connection.touch();
            pooled_connection.stream
        }
        None => {
            // 连接池没有可用连接，进行回退：立即建立新连接
            debug!("连接池无可用连接，立即建立到 {} 的新连接", remote_addr);
            METRICS.record_pool_miss(&selected_ip.to_string());
            
            let start_time = std::time::Instant::now();
            
            // 创建带keepalive的连接
            match pools::create_connection_with_keepalive(selected_ip, remote_port).await {
                Ok(socket) => {
                    // 连接成功，更新分数和指标
                    let connect_time = start_time.elapsed();
                    METRICS.record_connection_duration(connect_time);
                    
                    if let Some(mut score_data) = score_board.get_mut(&selected_ip) {
                        score_data.record_success(connect_time);
                    }
                    
                    debug!("立即连接到 {} 成功，耗时: {:?}", remote_addr, connect_time);
                    socket
                },
                Err(e) => {
                    // 连接失败，更新分数
                    if let Some(mut score_data) = score_board.get_mut(&selected_ip) {
                        score_data.record_failure();
                    }
                    return Err(anyhow::anyhow!("连接到远程服务器 {} 失败: {}", remote_addr, e));
                }
            }
        }
    };
    
    // 开始双向数据转发
    info!("开始在 {} 和 {} 之间转发数据", client_addr, remote_addr);
    match copy_bidirectional(&mut client_socket, &mut remote_socket).await {
        Ok((to_remote, to_client)) => {
            // 记录传输字节数
            METRICS.record_transfer_bytes(to_remote + to_client);
            
            info!(
                "连接关闭：客户端 -> 远程 {} 字节，远程 -> 客户端 {} 字节", 
                to_remote, to_client
            );
            Ok(())
        },
        Err(e) => {
            // 记录错误，但仍然返回错误，以便上层处理
            warn!("数据转发过程中连接中断: {}", e);
            Err(e).context("数据转发失败")
        }
    }
}

/// 从活跃IP列表中选择一个目标IP
///
/// 根据配置的负载均衡算法选择最适合的IP
async fn select_target_ip(active_remotes: &ActiveRemotes) -> Result<IpAddr> {
    // 获取活跃IP列表的读锁
    let ips = active_remotes.read().await;
    
    // 检查列表是否为空
    if ips.is_empty() {
        debug!("活跃IP列表为空，无法选择目标IP");
        return Err(anyhow::anyhow!("没有可用的活跃IP"));
    }
    
    debug!("当前活跃IP列表大小: {}", ips.len());
    
    // 目前使用简单的随机选择策略
    // 未来可以根据配置实现不同的负载均衡算法：
    // - least_connections: 选择当前活跃连接数最少的IP
    // - round_robin: 轮询选择
    // - random: 随机选择 (当前实现)
    let mut rng = rand::rng();
    match ips.choose(&mut rng) {
        Some(ip) => {
            debug!("通过随机算法选择目标IP: {}", ip);
            Ok(*ip)
        },
        None => {
            error!("无法从活跃IP列表中选择IP，列表大小: {}", ips.len());
            Err(anyhow::anyhow!("无法选择目标IP"))
        }
    }
}

/// 初始化日志系统
fn init_logging(logging_config: &config::LoggingConfig) -> Result<()> {
    // 解析日志级别
    let level = match logging_config.level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO, // 默认级别
    };
    
    // 根据配置设置日志格式
    if logging_config.format.to_lowercase() == "json" {
        // 使用JSON格式
        let json_logger = tracing_subscriber::fmt()
            .json()
            .with_env_filter(EnvFilter::from_default_env().add_directive(level.into()));
            
        // 根据配置设置日志输出目标
        match logging_config.output.to_lowercase().as_str() {
            "stdout" => {
                json_logger.with_writer(std::io::stdout).init();
            },
            "stderr" => {
                json_logger.with_writer(std::io::stderr).init();
            },
            file_path => {
                // 尝试创建日志文件
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(file_path)
                    .context(format!("无法打开日志文件: {}", file_path))?;
                    
                json_logger.with_writer(file).init();
            },
        }
    } else {
        // 使用可读文本格式（默认）
        let pretty_logger = tracing_subscriber::fmt()
            .pretty()
            .with_env_filter(EnvFilter::from_default_env().add_directive(level.into()));
            
        // 根据配置设置日志输出目标
        match logging_config.output.to_lowercase().as_str() {
            "stdout" => {
                pretty_logger.with_writer(std::io::stdout).init();
            },
            "stderr" => {
                pretty_logger.with_writer(std::io::stderr).init();
            },
            file_path => {
                // 尝试创建日志文件
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(file_path)
                    .context(format!("无法打开日志文件: {}", file_path))?;
                    
                pretty_logger.with_writer(file).init();
            },
        }
    }
    
    Ok(())
}
