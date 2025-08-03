use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;

// 顶级配置结构
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    /// 服务器配置
    pub server: ServerConfig,
    /// 日志配置
    pub logging: LoggingConfig,
    /// 指标监控配置
    pub metrics: MetricsConfig,
    /// 远程端点管理配置
    pub remotes: RemotesConfig,
    /// 连接池与负载均衡配置
    pub pools: PoolsConfig,
}

// 服务器配置
#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    /// 监听地址和端口
    pub listen_addr: SocketAddr,
}

// 日志配置
#[derive(Debug, Deserialize, Clone)]
pub struct LoggingConfig {
    /// 日志级别: trace, debug, info, warn, error
    pub level: String,
    /// 日志格式: text, json
    pub format: String,
    /// 日志输出位置: stdout, stderr, 或者文件路径
    pub output: String,
}

// 指标监控配置
#[derive(Debug, Deserialize, Clone)]
pub struct MetricsConfig {
    /// 是否启用指标监控
    pub enabled: bool,
    /// 指标服务监听地址
    pub listen_addr: SocketAddr,
    /// 指标HTTP路径
    pub path: String,
}

// 远程端点管理配置
#[derive(Debug, Deserialize, Clone)]
pub struct RemotesConfig {
    /// 默认远程端口
    pub default_remote_port: u16,
    /// IP源提供者配置
    pub provider: ProviderConfig,
    /// 评分探测配置
    pub probing: ProbingConfig,
    /// 评分模型配置
    pub scoring: ScoringConfig,
    /// 选择器策略配置
    pub selector: SelectorConfig,
}

// IP源提供者配置
#[derive(Debug, Deserialize, Clone)]
pub struct ProviderConfig {
    /// 源类型: file, http, dns_srv
    #[serde(rename = "type")]
    pub provider_type: String,
    /// 文件提供者配置
    pub file: Option<FileProviderConfig>,
    // 将来可能会有更多类型的提供者配置
}

// 文件提供者配置
#[derive(Debug, Deserialize, Clone)]
pub struct FileProviderConfig {
    /// IP列表文件路径
    pub path: PathBuf,
    /// 是否监视文件变动
    pub watch: bool,
}

// 评分探测配置
#[derive(Debug, Deserialize, Clone)]
pub struct ProbingConfig {
    /// 探测间隔
    #[serde(with = "humantime_serde")]
    pub interval: std::time::Duration,
    /// 探测超时时间
    #[serde(with = "humantime_serde")]
    pub timeout: std::time::Duration,
    /// 每次探测的候选IP数量
    pub probe_candidate_count: usize,
    /// 初始探测配置
    pub initial: Option<InitialProbingConfig>,
}

// 初始探测配置
#[derive(Debug, Deserialize, Clone)]
pub struct InitialProbingConfig {
    /// 是否启用启动时的初始探测
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 初始探测的最大并发数
    #[serde(default = "default_initial_concurrency")]
    pub max_concurrency: usize,
    /// 初始探测的超时时间（如果未设置，使用常规探测超时）
    #[serde(with = "humantime_serde", default)]
    pub timeout: Option<std::time::Duration>,
}

fn default_true() -> bool {
    true
}

fn default_initial_concurrency() -> usize {
    100
}

// 评分模型配置
#[derive(Debug, Deserialize, Clone)]
pub struct ScoringConfig {
    /// 权重配置
    #[allow(dead_code)]
    pub weights: ScoringWeightsConfig,
    /// 延迟评分配置
    pub latency: LatencyConfig,
    /// 抖动评分配置
    pub jitter: JitterConfig,
    /// 成功率评分配置
    pub success_rate: SuccessRateConfig,
    /// 失败惩罚配置
    pub failure_penalty: FailurePenaltyConfig,
    /// 历史稳定性奖励配置
    pub historical_bonus: HistoricalBonusConfig,
}

// 权重配置
#[derive(Debug, Deserialize, Clone)]
pub struct ScoringWeightsConfig {
    /// 延迟权重
    #[allow(dead_code)]
    pub latency: f64,
    /// 抖动权重
    #[allow(dead_code)]
    pub jitter: f64,
    /// 成功率权重
    #[allow(dead_code)]
    pub success_rate: f64,
}

// 延迟评分配置
#[derive(Debug, Deserialize, Clone)]
pub struct LatencyConfig {
    /// 最大得分
    pub max_score: f64,
    /// EWMA平滑因子
    pub ewma_alpha: f64,
    /// 基准延迟
    #[serde(with = "humantime_serde")]
    pub base_latency: std::time::Duration,
    /// 最大可接受延迟
    #[serde(with = "humantime_serde")]
    pub max_acceptable_latency: std::time::Duration,
}

// 抖动评分配置
#[derive(Debug, Deserialize, Clone)]
pub struct JitterConfig {
    /// 最大得分
    pub max_score: f64,
    /// EWMA平滑因子
    pub ewma_alpha: f64,
    /// 基准抖动
    #[serde(with = "humantime_serde")]
    pub base_jitter: std::time::Duration,
    /// 最大可接受抖动
    #[serde(with = "humantime_serde")]
    pub max_acceptable_jitter: std::time::Duration,
}

// 成功率评分配置
#[derive(Debug, Deserialize, Clone)]
pub struct SuccessRateConfig {
    /// 最大得分
    pub max_score: f64,
    /// EWMA平滑因子
    pub ewma_alpha: f64,
}

// 失败惩罚配置
#[derive(Debug, Deserialize, Clone)]
pub struct FailurePenaltyConfig {
    /// 基础惩罚
    pub base_penalty: f64,
    /// 惩罚指数增长因子
    pub exponent_factor: f64,
    /// 最大惩罚
    pub max_penalty: f64,
    /// 每次探测恢复分数
    pub recovery_per_check: f64,
}

// 历史稳定性奖励配置
#[derive(Debug, Deserialize, Clone)]
pub struct HistoricalBonusConfig {
    /// 最大奖励
    pub max_bonus: f64,
    /// 每点奖励所需的连续成功次数
    pub checks_per_point: u64,
}

// 选择器策略配置
#[derive(Debug, Deserialize, Clone)]
pub struct SelectorConfig {
    /// 评估周期
    #[serde(with = "humantime_serde")]
    pub evaluation_interval: std::time::Duration,
    /// 活跃集大小
    pub active_set_size: usize,
    /// 最低分数阈值
    pub min_score_threshold: f64,
    /// 切换策略
    pub switching: SwitchingPolicyConfig,
}

// 切换策略配置
#[derive(Debug, Deserialize, Clone)]
pub struct SwitchingPolicyConfig {
    /// 防抖间隔：两次切换之间的最小时间
    #[serde(with = "humantime_serde")]
    pub debounce_interval: std::time::Duration,
    /// 分数改进阈值：只有当新IP集合的平均分比当前集合高出至少这个值时才切换
    pub score_improvement_threshold: f64,
}

// 连接池与负载均衡配置
#[derive(Debug, Deserialize, Clone)]
pub struct PoolsConfig {
    /// 负载均衡算法
    pub algorithm: String,
    /// 连接池通用配置
    pub common: PoolCommonConfig,
    /// 连接池策略
    pub strategy: PoolStrategyConfig,
}

// 连接池通用配置
#[derive(Debug, Deserialize, Clone)]
pub struct PoolCommonConfig {
    /// 连接超时
    #[serde(with = "humantime_serde")]
    pub dial_timeout: std::time::Duration,
    /// 空闲超时
    #[serde(with = "humantime_serde")]
    pub idle_timeout: std::time::Duration,
    /// 排空超时
    #[serde(with = "humantime_serde")]
    #[allow(dead_code)]
    pub drain_timeout: std::time::Duration,
}

// 连接池策略配置
#[derive(Debug, Deserialize, Clone)]
pub struct PoolStrategyConfig {
    /// 策略类型: static, dynamic
    #[serde(rename = "type")]
    pub strategy_type: String,
    /// 静态策略配置
    pub static_: Option<StaticStrategyConfig>,
    /// 动态策略配置
    pub dynamic: Option<DynamicStrategyConfig>,
}

// 静态策略配置
#[derive(Debug, Deserialize, Clone)]
pub struct StaticStrategyConfig {
    /// 每个远程IP的连接池大小
    pub size_per_remote: usize,
}

// 动态策略配置
#[derive(Debug, Deserialize, Clone)]
pub struct DynamicStrategyConfig {
    /// 最小连接池大小
    pub min_size: usize,
    /// 最大连接池大小
    pub max_size: usize,
    /// 伸缩配置
    pub scaling: ScalingConfig,
}

// 伸缩配置
#[derive(Debug, Deserialize, Clone)]
pub struct ScalingConfig {
    /// 调整间隔
    #[serde(with = "humantime_serde")]
    pub interval: std::time::Duration,
    /// 目标缓冲比例
    pub target_buffer_ratio: f64,
    /// 扩容增量
    pub scale_up_increment: usize,
    /// 缩容增量
    pub scale_down_increment: usize,
}
