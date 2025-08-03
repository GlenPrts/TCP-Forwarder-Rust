mod config;

use anyhow::{Context, Result};
use config::{Config};
use std::path::Path;
use tracing::{info, Level};
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
    
    // TODO: 在这里实现服务器逻辑
    
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
