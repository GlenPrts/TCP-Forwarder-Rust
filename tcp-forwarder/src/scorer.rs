use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use dashmap::DashMap;

/// EWMA (指数加权移动平均)实现
#[derive(Debug, Clone)]
pub struct Ewma {
    /// 当前EWMA值
    value: f64,
    /// 平滑因子，取值范围(0.0, 1.0)
    /// 值越大，对新样本越敏感；值越小，越平滑
    alpha: f64,
}

impl Ewma {
    /// 创建新的EWMA实例
    pub fn new(initial_value: f64, alpha: f64) -> Self {
        Self {
            value: initial_value,
            alpha,
        }
    }

    /// 添加新样本并更新EWMA值
    pub fn update(&mut self, new_sample: f64) -> f64 {
        self.value = self.alpha * new_sample + (1.0 - self.alpha) * self.value;
        self.value
    }

    /// 获取当前EWMA值
    pub fn value(&self) -> f64 {
        self.value
    }
}

/// IP评分数据
#[derive(Debug, Clone)]
pub struct ScoreData {
    /// IP地址
    pub ip: IpAddr,
    /// 远程端口
    pub port: u16,
    /// 延迟EWMA (毫秒)
    pub latency_ewma: Ewma,
    /// 抖动EWMA (毫秒)
    pub jitter_ewma: Ewma,
    /// 成功率EWMA (0.0-1.0)
    pub success_rate_ewma: Ewma,
    /// 当前连续失败次数
    pub consecutive_failures: u32,
    /// 当前失败惩罚分数
    pub failure_penalty: f64,
    /// 历史稳定性奖励分数
    pub historical_bonus: f64,
    /// 连续成功次数（用于计算历史稳定性奖励）
    pub consecutive_successes: u64,
    /// 最后一次探测时间
    pub last_probed: Option<Instant>,
    /// 最后一次被选为活跃IP的时间
    pub last_selected: Option<Instant>,
}

impl ScoreData {
    /// 创建新的ScoreData实例
    pub fn new(ip: IpAddr, port: u16, config: &crate::config::ScoringConfig) -> Self {
        Self {
            ip,
            port,
            latency_ewma: Ewma::new(
                config.latency.max_acceptable_latency.as_millis() as f64 / 2.0, 
                config.latency.ewma_alpha
            ),
            jitter_ewma: Ewma::new(
                config.jitter.max_acceptable_jitter.as_millis() as f64 / 2.0,
                config.jitter.ewma_alpha
            ),
            success_rate_ewma: Ewma::new(0.5, config.success_rate.ewma_alpha),
            consecutive_failures: 0,
            failure_penalty: 0.0,
            historical_bonus: 0.0,
            consecutive_successes: 0,
            last_probed: None,
            last_selected: None,
        }
    }

    /// 计算总分
    pub fn calculate_score(&self, config: &crate::config::ScoringConfig) -> f64 {
        // 计算延迟得分
        let latency_ms = self.latency_ewma.value();
        let base_latency_ms = config.latency.base_latency.as_millis() as f64;
        let max_latency_ms = config.latency.max_acceptable_latency.as_millis() as f64;
        
        let latency_score = if latency_ms <= base_latency_ms {
            // 延迟低于基准，满分
            config.latency.max_score
        } else if latency_ms >= max_latency_ms {
            // 延迟高于最大可接受值，0分
            0.0
        } else {
            // 线性插值计算分数
            let ratio = (max_latency_ms - latency_ms) / (max_latency_ms - base_latency_ms);
            ratio * config.latency.max_score
        };

        // 计算抖动得分
        let jitter_ms = self.jitter_ewma.value();
        let base_jitter_ms = config.jitter.base_jitter.as_millis() as f64;
        let max_jitter_ms = config.jitter.max_acceptable_jitter.as_millis() as f64;
        
        let jitter_score = if jitter_ms <= base_jitter_ms {
            // 抖动低于基准，满分
            config.jitter.max_score
        } else if jitter_ms >= max_jitter_ms {
            // 抖动高于最大可接受值，0分
            0.0
        } else {
            // 线性插值计算分数
            let ratio = (max_jitter_ms - jitter_ms) / (max_jitter_ms - base_jitter_ms);
            ratio * config.jitter.max_score
        };

        // 计算成功率得分
        let success_rate = self.success_rate_ewma.value();
        let success_score = success_rate * config.success_rate.max_score;

        // 总分 = 各项得分之和 - 失败惩罚 + 历史稳定性奖励
        let total_score = latency_score + jitter_score + success_score - self.failure_penalty + self.historical_bonus;
        
        // 分数不应该小于0
        total_score.max(0.0)
    }

    /// 更新探测结果
    pub fn update_probe_result(
        &mut self, 
        success: bool, 
        latency: Option<Duration>, 
        config: &crate::config::ScoringConfig
    ) {
        self.last_probed = Some(Instant::now());
        
        if success {
            // 成功连接
            // 更新成功率
            self.success_rate_ewma.update(1.0);
            
            // 更新延迟和抖动（如果有测量值）
            if let Some(latency) = latency {
                let latency_ms = latency.as_millis() as f64;
                
                // 计算抖���：当前延迟与平均延迟的差异绝对值
                let old_latency = self.latency_ewma.value();
                let jitter = (latency_ms - old_latency).abs();
                
                // 更新延迟和抖动EWMA
                self.latency_ewma.update(latency_ms);
                self.jitter_ewma.update(jitter);
            }
            
            // 恢复失败惩罚（如果有）
            if self.failure_penalty > 0.0 {
                self.failure_penalty = (self.failure_penalty - config.failure_penalty.recovery_per_check).max(0.0);
            }
            
            // 更新连续成功/失败计数
            self.consecutive_failures = 0;
            self.consecutive_successes += 1;
            
            // 更新历史奖励
            if self.consecutive_successes % config.historical_bonus.checks_per_point == 0 {
                let new_bonus = (self.historical_bonus + 1.0).min(config.historical_bonus.max_bonus);
                self.historical_bonus = new_bonus;
            }
        } else {
            // 连接失败
            // 更新成功率
            self.success_rate_ewma.update(0.0);
            
            // 更新连续失败计数并计算惩罚
            self.consecutive_failures += 1;
            self.consecutive_successes = 0;
            
            // 应用失败惩罚：按指数增长
            let base_penalty = config.failure_penalty.base_penalty;
            let factor = config.failure_penalty.exponent_factor;
            let max_penalty = config.failure_penalty.max_penalty;
            
            // 惩罚 = 基础惩罚 * (因子 ^ (连续失败次数 - 1))
            let penalty = if self.consecutive_failures == 1 {
                base_penalty
            } else {
                base_penalty * factor.powi((self.consecutive_failures - 1) as i32)
            };
            
            self.failure_penalty = penalty.min(max_penalty);
            
            // 连续失败次数过多时，重置历史奖励
            if self.consecutive_failures >= 5 {
                self.historical_bonus = 0.0;
            }
        }
    }
}

/// 评分板类型，用于全局共享IP评分数据
pub type ScoreBoard = Arc<DashMap<IpAddr, ScoreData>>;

/// 创建一个新的评分板
pub fn create_score_board() -> ScoreBoard {
    Arc::new(DashMap::new())
}
