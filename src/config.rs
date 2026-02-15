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

    #[error("initial_scan_mask must be between 8 and 32")]
    InvalidInitialScanMask,

    #[error("promising_subnet_percent must be between 0 and 1, got {0}")]
    InvalidPromisingSubnetPercent(f64),

    #[error("background_scan interval_secs must be >= 60")]
    InvalidBgScanInterval,

    #[error("background_scan concurrency must be > 0")]
    InvalidBgScanConcurrency,
}

/// 后台定时扫描配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundScanConfig {
    /// 是否启用后台定时扫描
    #[serde(default)]
    pub enabled: bool,

    /// 扫描间隔（秒），最小 60 秒
    #[serde(default = "default_bg_scan_interval")]
    pub interval_secs: u64,

    /// 后台扫描并发数
    #[serde(default = "default_bg_scan_concurrency")]
    pub concurrency: usize,
}

fn default_bg_scan_interval() -> u64 {
    3600
}

fn default_bg_scan_concurrency() -> usize {
    100
}

impl Default for BackgroundScanConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: default_bg_scan_interval(),
            concurrency: default_bg_scan_concurrency(),
        }
    }
}

impl BackgroundScanConfig {
    /// 验证后台扫描配置
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.interval_secs < 60 {
            return Err(ConfigError::InvalidBgScanInterval.into());
        }
        if self.concurrency == 0 {
            return Err(ConfigError::InvalidBgScanConcurrency.into());
        }
        Ok(())
    }
}

/// 扫描策略类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScanStrategyType {
    /// 全面扫描
    #[default]
    FullScan,
    /// 自适应扫描
    Adaptive,
}

/// 扫描策略配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanStrategyConfig {
    /// 策略类型: "full_scan" 或 "adaptive"
    #[serde(default)]
    pub r#type: ScanStrategyType,

    /// [自适应] 阶段一使用的CIDR掩码 (例如 20)
    #[serde(default = "default_initial_scan_mask")]
    pub initial_scan_mask: u8,

    /// [自适应] 阶段一每个大子网的采样数 (例如 1)
    #[serde(default = "default_initial_samples_per_subnet")]
    pub initial_samples_per_subnet: usize,

    /// [自适应] 阶段二筛选“热点区域”时使用的百分比 (0.0-1.0)
    #[serde(default = "default_promising_subnet_percent")]
    pub promising_subnet_percent: f64,

    /// [自适应] 阶段三精细扫描时每个 /24 子网的采样数
    #[serde(default = "default_focused_samples_per_subnet")]
    pub focused_samples_per_subnet: usize,
}

fn default_initial_scan_mask() -> u8 {
    20
}

fn default_initial_samples_per_subnet() -> usize {
    1
}

fn default_promising_subnet_percent() -> f64 {
    0.2
}

fn default_focused_samples_per_subnet() -> usize {
    3
}

impl Default for ScanStrategyConfig {
    fn default() -> Self {
        Self {
            r#type: ScanStrategyType::default(),
            initial_scan_mask: default_initial_scan_mask(),
            initial_samples_per_subnet: default_initial_samples_per_subnet(),
            promising_subnet_percent: default_promising_subnet_percent(),
            focused_samples_per_subnet: default_focused_samples_per_subnet(),
        }
    }
}

impl ScanStrategyConfig {
    /// 验证扫描策略配置
    pub fn validate(&self) -> Result<()> {
        if self.r#type == ScanStrategyType::Adaptive {
            if self.initial_scan_mask < 8 || self.initial_scan_mask > 32 {
                return Err(ConfigError::InvalidInitialScanMask.into());
            }
            if self.promising_subnet_percent <= 0.0 || self.promising_subnet_percent >= 1.0 {
                return Err(ConfigError::InvalidPromisingSubnetPercent(
                    self.promising_subnet_percent,
                )
                .into());
            }
        }
        Ok(())
    }
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

    /// 扫描策略配置
    #[serde(default)]
    pub scan_strategy: ScanStrategyConfig,

    /// 后台定时扫描配置
    #[serde(default)]
    pub background_scan: BackgroundScanConfig,
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
            scan_strategy: ScanStrategyConfig::default(),
            background_scan: BackgroundScanConfig::default(),
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

        // 验证 URL 格式
        if let Err(e) = reqwest::Url::parse(&self.trace_url) {
            return Err(anyhow::anyhow!("Invalid trace_url: {}", e));
        }

        if self.ip_store_file.is_empty() {
            return Err(ConfigError::EmptyIpStoreFile.into());
        }

        // 验证 ASN URL（如果提供）
        if !self.asn_url.is_empty() {
            if let Err(e) = reqwest::Url::parse(&self.asn_url) {
                return Err(anyhow::anyhow!("Invalid asn_url: {}", e));
            }
        }

        self.scan_strategy.validate()?;
        self.background_scan.validate()?;

        Ok(())
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
    fn test_adaptive_scan_config_validation() {
        let mut config = AppConfig::default();
        config.scan_strategy.r#type = ScanStrategyType::Adaptive;

        // Valid
        config.scan_strategy.initial_scan_mask = 20;
        config.scan_strategy.promising_subnet_percent = 0.2;
        assert!(config.validate().is_ok());

        // Invalid mask
        config.scan_strategy.initial_scan_mask = 7;
        assert!(config.validate().is_err());
        config.scan_strategy.initial_scan_mask = 33;
        assert!(config.validate().is_err());
        config.scan_strategy.initial_scan_mask = 20; // reset

        // Invalid percent
        config.scan_strategy.promising_subnet_percent = 0.0;
        assert!(config.validate().is_err());
        config.scan_strategy.promising_subnet_percent = 1.0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_background_scan_config_validation() {
        let mut config = AppConfig::default();

        // 禁用时不验证
        config.background_scan.enabled = false;
        config.background_scan.interval_secs = 0;
        assert!(config.validate().is_ok());

        // 启用时验证间隔
        config.background_scan.enabled = true;
        config.background_scan.interval_secs = 59;
        config.background_scan.concurrency = 100;
        assert!(config.validate().is_err());

        // 合法间隔
        config.background_scan.interval_secs = 60;
        assert!(config.validate().is_ok());

        // 并发数为 0
        config.background_scan.concurrency = 0;
        assert!(config.validate().is_err());
    }
}
