use anyhow::{Context, Result};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::Path;
use thiserror::Error;

/// 配置错误类型
#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("selection_top_k_percent must be between 0 and 1, got {0}")]
    InvalidTopKPercent(f64),

    #[error("selection_random_n_subnets must be greater than 0")]
    InvalidNSubnets,

    #[error("selection_random_m_ips must be greater than 0")]
    InvalidMIps,

    #[error("target_port must be greater than 0")]
    InvalidTargetPort,

    #[error("cidr_list cannot be empty")]
    EmptyCidrList,

    #[error("trace_url cannot be empty")]
    EmptyTraceUrl,

    #[error("ip_store_file cannot be empty")]
    EmptyIpStoreFile,
}

/// 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// 目标端口（如 Cloudflare 的 443）
    pub target_port: u16,

    /// 目标数据中心代码列表（如 "SJC", "LAX"）
    /// 如果为空，则不限制数据中心
    #[serde(default)]
    pub target_colos: Vec<String>,

    /// 初始扫描的 IP 网段列表
    pub cidr_list: Vec<IpNet>,

    /// TCP 转发服务监听地址
    pub bind_addr: SocketAddr,

    /// Web 监控服务监听地址
    pub web_addr: SocketAddr,

    /// 用于测试 IP 延迟和获取 Colo 信息的 URL
    pub trace_url: String,

    /// 用于动态更新 CIDR 列表的 ASN URL
    #[serde(default)]
    pub asn_url: String,

    /// 扫描结果保存路径
    pub ip_store_file: String,

    /// 仅使用评分最高的前 k% 的 IP 段（0.0-1.0）
    #[serde(default = "default_top_k_percent")]
    pub selection_top_k_percent: f64,

    /// 每次建立连接时随机选取的 IP 段数量
    #[serde(default = "default_n_subnets")]
    pub selection_random_n_subnets: usize,

    /// 每个选中的 IP 段内随机生成的 IP 数量
    #[serde(default = "default_m_ips")]
    pub selection_random_m_ips: usize,
}

fn default_top_k_percent() -> f64 {
    0.1
}

fn default_n_subnets() -> usize {
    3
}

fn default_m_ips() -> usize {
    2
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            target_port: 443,
            target_colos: vec!["SJC".to_string(), "LAX".to_string()],
            cidr_list: vec![
                "104.16.0.0/12".parse().unwrap(),
                "172.64.0.0/13".parse().unwrap(),
            ],
            bind_addr: "0.0.0.0:8080".parse().unwrap(),
            web_addr: "0.0.0.0:3000".parse().unwrap(),
            trace_url: "http://engage.cloudflareclient.com/cdn-cgi/trace".to_string(),
            asn_url: "https://asn.0x01111110.com/13335?4".to_string(),
            ip_store_file: "subnet_results.json".to_string(),
            selection_top_k_percent: default_top_k_percent(),
            selection_random_n_subnets: default_n_subnets(),
            selection_random_m_ips: default_m_ips(),
        }
    }
}

impl AppConfig {
    /// 从文件加载配置
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path)
            .with_context(|| format!("Failed to open config file: {}", path.display()))?;
        let reader = BufReader::new(file);
        let config: Self = serde_json::from_reader(reader)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// 验证配置参数
    pub fn validate(&self) -> Result<()> {
        if self.target_port == 0 {
            return Err(ConfigError::InvalidTargetPort.into());
        }

        if self.selection_top_k_percent <= 0.0 || self.selection_top_k_percent > 1.0 {
            return Err(ConfigError::InvalidTopKPercent(self.selection_top_k_percent).into());
        }

        if self.selection_random_n_subnets == 0 {
            return Err(ConfigError::InvalidNSubnets.into());
        }

        if self.selection_random_m_ips == 0 {
            return Err(ConfigError::InvalidMIps.into());
        }

        if self.cidr_list.is_empty() {
            return Err(ConfigError::EmptyCidrList.into());
        }

        if self.trace_url.is_empty() {
            return Err(ConfigError::EmptyTraceUrl.into());
        }

        if self.ip_store_file.is_empty() {
            return Err(ConfigError::EmptyIpStoreFile.into());
        }

        Ok(())
    }

    /// 获取总并发连接尝试数
    pub fn total_concurrent_connections(&self) -> usize {
        self.selection_random_n_subnets * self.selection_random_m_ips
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_is_valid() {
        let config = AppConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_invalid_top_k_percent() {
        let mut config = AppConfig::default();
        config.selection_top_k_percent = 1.5;
        assert!(config.validate().is_err());

        config.selection_top_k_percent = 0.0;
        assert!(config.validate().is_err());

        config.selection_top_k_percent = -0.1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_n_subnets() {
        let mut config = AppConfig::default();
        config.selection_random_n_subnets = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_m_ips() {
        let mut config = AppConfig::default();
        config.selection_random_m_ips = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_total_concurrent_connections() {
        let config = AppConfig::default();
        assert_eq!(
            config.total_concurrent_connections(),
            config.selection_random_n_subnets * config.selection_random_m_ips
        );
    }
}