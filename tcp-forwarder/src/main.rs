mod config;
mod scorer;
mod probing;

use anyhow::{Context, Result};
use config::{Config};
use std::path::Path;
use std::net::{SocketAddr, IpAddr};
use std::sync::Arc;
use std::time::Instant;
use std::fs::File;
use std::io::{BufRead, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::io::copy_bidirectional;
use tracing::{info, warn, error, debug, Level, instrument};
use tracing_subscriber::EnvFilter;

/// 主函数
#[tokio::main]
async fn main() -> Result<()> {
    // 加载配置文件
    let config = load_config("config.yaml")?;
    
    // 初始化日志系统
    init_logging(&config.logging)?;
    
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
    
    // 启动探测任务
    let probing_score_board = score_board.clone();
    let probing_config = config.remotes.clone();
    tokio::spawn(async move {
        probing::probing_task(probing_score_board, probing_config).await;
    });
    
    // 硬编码一个远程地址（在阶段2中仍使用，但实际上我们已经有了IP列表）
    let remote_addr = "5.10.214.29:443";
    
    // 创建TCP监听器
    let listener = TcpListener::bind(config.server.listen_addr)
        .await
        .context("无法绑定到指定地址")?;
    
    info!("TCP服务器已启动，正在监听: {}", config.server.listen_addr);
    
    // 主循环：接受新的连接并处理
    loop {
        // 接受新的连接
        let (client_socket, client_addr) = match listener.accept().await {
            Ok((socket, addr)) => (socket, addr),
            Err(e) => {
                error!("接受连接失败: {}", e);
                continue;
            }
        };
        
        info!("接受到来自 {} 的连接", client_addr);
        
        // 使用硬编码的远程地址
        let remote_addr_clone = remote_addr.to_string();
        
        // 为每个连接创建新的异步任务处理
        tokio::spawn(async move {
            if let Err(e) = handle_connection(client_socket, client_addr, &remote_addr_clone).await {
                error!(client_addr = %client_addr, "处理连接时出错: {}", e);
            }
        });
    }
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
#[instrument(skip(client_socket), fields(remote_addr = %remote_addr))]
async fn handle_connection(
    mut client_socket: TcpStream, 
    client_addr: SocketAddr, 
    remote_addr: &str
) -> Result<()> {
    // 记录开始处理连接
    debug!("开始处理来自 {} 的连接，目标地址: {}", client_addr, remote_addr);
    
    // 连接到远程服务器
    let start_time = std::time::Instant::now();
    let mut remote_socket = TcpStream::connect(remote_addr)
        .await
        .context(format!("连接到远程服务器 {} 失败", remote_addr))?;
    
    let connect_time = start_time.elapsed();
    debug!("连接到 {} 成功，耗时: {:?}", remote_addr, connect_time);
    
    // 开始双向数据转发
    info!("开始在 {} 和 {} 之间转发数据", client_addr, remote_addr);
    match copy_bidirectional(&mut client_socket, &mut remote_socket).await {
        Ok((to_remote, to_client)) => {
            info!(
                "连接关闭：客户端 -> 远程 {} 字节，远程 -> 客户端 {} 字节", 
                to_remote, to_client
            );
            Ok(())
        },
        Err(e) => {
            // 记录错误，但仍然返回Ok，因为这是预期的错误（例如客户端关闭连接）
            warn!("数据转发过程中连接中断: {}", e);
            Err(e).context("数据转发失败")
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
