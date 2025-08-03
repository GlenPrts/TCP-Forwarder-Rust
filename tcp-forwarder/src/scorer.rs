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

    /// 计算总分（归一化到0-100）
    pub fn calculate_score(&self, config: &crate::config::ScoringConfig) -> f64 {
        // 计算延迟得分（0-1范围内的归一化分数）
        let latency_ms = self.latency_ewma.value();
        let base_latency_ms = config.latency.base_latency.as_millis() as f64;
        let max_latency_ms = config.latency.max_acceptable_latency.as_millis() as f64;
        
        let latency_normalized = if latency_ms <= base_latency_ms {
            // 延迟低于基准，满分
            1.0
        } else if latency_ms >= max_latency_ms {
            // 延迟高于最大可接受值，0分
            0.0
        } else {
            // 线性插值计算分数
            (max_latency_ms - latency_ms) / (max_latency_ms - base_latency_ms)
        };

        // 计算抖动得分（0-1范围内的归一化分数）
        let jitter_ms = self.jitter_ewma.value();
        let base_jitter_ms = config.jitter.base_jitter.as_millis() as f64;
        let max_jitter_ms = config.jitter.max_acceptable_jitter.as_millis() as f64;
        
        let jitter_normalized = if jitter_ms <= base_jitter_ms {
            // 抖动低于基准，满分
            1.0
        } else if jitter_ms >= max_jitter_ms {
            // 抖动高于最大可接受值，0分
            0.0
        } else {
            // 线性插值计算分数
            (max_jitter_ms - jitter_ms) / (max_jitter_ms - base_jitter_ms)
        };

        // 计算成功率得分（0-1范围内的归一化分数）
        let success_rate_normalized = self.success_rate_ewma.value().clamp(0.0, 1.0);

        // 使用权重配置计算加权分数
        let weighted_score = latency_normalized * config.weights.latency +
                           jitter_normalized * config.weights.jitter +
                           success_rate_normalized * config.weights.success_rate;

        // 将加权分数转换为0-100范围（权重之和应该为1.0）
        let base_score = weighted_score * 100.0;
        
        // 应用失败惩罚（从基础分中扣除）
        let score_after_penalty = (base_score - self.failure_penalty).max(0.0);
        
        // 应用历史稳定性奖励（但不超过理论最大分数）
        let max_possible_score = config.latency.max_score + config.jitter.max_score + config.success_rate.max_score;
        let final_score = (score_after_penalty + self.historical_bonus).min(max_possible_score);
        
        // 确保分数在合理范围内
        final_score.clamp(0.0, max_possible_score)
    }

    /// 获取总分（简化版本，归一化到0-100）
    /// 注意：这是一个简化版本，使用默认配置参数，仅用于快速评估
    pub fn total_score(&self) -> f64 {
        // 使用默认权重：延迟45%，抖动15%，成功率40%
        const DEFAULT_LATENCY_WEIGHT: f64 = 0.45;
        const DEFAULT_JITTER_WEIGHT: f64 = 0.15;
        const DEFAULT_SUCCESS_RATE_WEIGHT: f64 = 0.40;
        
        // 延迟分数：延迟越低分数越高
        // 假设理想延迟是50ms，最大可接受延迟是500ms
        let latency_ms = self.latency_ewma.value();
        let latency_normalized = if latency_ms <= 50.0 {
            1.0 // 满分
        } else if latency_ms >= 500.0 {
            0.0 // 0分
        } else {
            (500.0 - latency_ms) / (500.0 - 50.0)
        };
        
        // 抖动分数：抖动越低分数越高
        // 假设理想抖动是10ms，最大可接受抖动是80ms
        let jitter_ms = self.jitter_ewma.value();
        let jitter_normalized = if jitter_ms <= 10.0 {
            1.0 // 满分
        } else if jitter_ms >= 80.0 {
            0.0 // 0分
        } else {
            (80.0 - jitter_ms) / (80.0 - 10.0)
        };
        
        // 成功率分数（已经在0-1范围内）
        let success_rate_normalized = self.success_rate_ewma.value().clamp(0.0, 1.0);
        
        // 使用权重计算加权分数
        let weighted_score = latency_normalized * DEFAULT_LATENCY_WEIGHT +
                           jitter_normalized * DEFAULT_JITTER_WEIGHT +
                           success_rate_normalized * DEFAULT_SUCCESS_RATE_WEIGHT;
        
        // 转换为0-100范围
        let base_score = weighted_score * 100.0;
        
        // 应用惩罚和奖励
        let score_after_penalty = (base_score - self.failure_penalty).max(0.0);
        let final_score = (score_after_penalty + self.historical_bonus).min(100.0);
        
        // 确保分数在0-100范围内
        final_score.clamp(0.0, 100.0)
    }

    /// 获取详细的评分信息（用于调试和监控）
    pub fn get_score_details(&self, config: &crate::config::ScoringConfig) -> ScoreDetails {
        let latency_ms = self.latency_ewma.value();
        let base_latency_ms = config.latency.base_latency.as_millis() as f64;
        let max_latency_ms = config.latency.max_acceptable_latency.as_millis() as f64;
        
        let latency_normalized = if latency_ms <= base_latency_ms {
            1.0
        } else if latency_ms >= max_latency_ms {
            0.0
        } else {
            (max_latency_ms - latency_ms) / (max_latency_ms - base_latency_ms)
        };
        let latency_score = latency_normalized * config.latency.max_score * config.weights.latency;

        let jitter_ms = self.jitter_ewma.value();
        let base_jitter_ms = config.jitter.base_jitter.as_millis() as f64;
        let max_jitter_ms = config.jitter.max_acceptable_jitter.as_millis() as f64;
        
        let jitter_normalized = if jitter_ms <= base_jitter_ms {
            1.0
        } else if jitter_ms >= max_jitter_ms {
            0.0
        } else {
            (max_jitter_ms - jitter_ms) / (max_jitter_ms - base_jitter_ms)
        };
        let jitter_score = jitter_normalized * config.jitter.max_score * config.weights.jitter;

        let success_rate = self.success_rate_ewma.value().clamp(0.0, 1.0);
        let success_score = success_rate * config.success_rate.max_score * config.weights.success_rate;

        let base_score = latency_score + jitter_score + success_score;
        let score_after_penalty = (base_score - self.failure_penalty).max(0.0);
        let max_possible_score = config.latency.max_score + config.jitter.max_score + config.success_rate.max_score;
        let final_score = (score_after_penalty + self.historical_bonus).min(max_possible_score).clamp(0.0, max_possible_score);

        ScoreDetails {
            ip: self.ip,
            port: self.port,
            latency_ms,
            latency_score,
            jitter_ms,
            jitter_score,
            success_rate,
            success_score,
            base_score,
            failure_penalty: self.failure_penalty,
            historical_bonus: self.historical_bonus,
            final_score,
            consecutive_failures: self.consecutive_failures,
            consecutive_successes: self.consecutive_successes,
        }
    }

    /// 记录连接成功
    pub fn record_success(&mut self, latency: Duration) {
        // 简化版的成功记录，用于实际转发连接
        self.success_rate_ewma.update(1.0);
        let latency_ms = latency.as_millis() as f64;
        let old_latency = self.latency_ewma.value();
        let jitter = (latency_ms - old_latency).abs();
        
        self.latency_ewma.update(latency_ms);
        self.jitter_ewma.update(jitter);
        
        self.consecutive_failures = 0;
        self.consecutive_successes += 1;
    }
    
    /// 记录连接失败
    pub fn record_failure(&mut self) {
        // 简化版的失败记录，用于实际转发连接
        self.success_rate_ewma.update(0.0);
        self.consecutive_failures += 1;
        self.consecutive_successes = 0;
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
                
                // 计算抖动：当前延迟与平均延迟的差异绝对值
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

    /// 更新最后被选中的时间
    pub fn update_last_selected(&mut self) {
        self.last_selected = Some(Instant::now());
    }
}

/// 评分详情结构体，用于调试和监控
#[derive(Debug, Clone)]
pub struct ScoreDetails {
    pub ip: IpAddr,
    #[allow(dead_code)]
    pub port: u16,
    pub latency_ms: f64,
    pub latency_score: f64,
    pub jitter_ms: f64,
    pub jitter_score: f64,
    pub success_rate: f64,
    pub success_score: f64,
    #[allow(dead_code)]
    pub base_score: f64,
    pub failure_penalty: f64,
    pub historical_bonus: f64,
    pub final_score: f64,
    pub consecutive_failures: u32,
    pub consecutive_successes: u64,
}

impl ScoreDetails {
    /// 格式化为易读的字符串
    pub fn format(&self) -> String {
        format!(
            "IP: {} | 总分: {:.1} | 延迟: {:.1}ms({:.1}分) | 抖动: {:.1}ms({:.1}分) | 成功率: {:.1}%({:.1}分) | 惩罚: {:.1} | 奖励: {:.1} | 连败: {} | 连胜: {}",
            self.ip,
            self.final_score,
            self.latency_ms,
            self.latency_score,
            self.jitter_ms,
            self.jitter_score,
            self.success_rate * 100.0,
            self.success_score,
            self.failure_penalty,
            self.historical_bonus,
            self.consecutive_failures,
            self.consecutive_successes
        )
    }
}

/// 评分板类型，用于全局共享IP评分数据
pub type ScoreBoard = Arc<DashMap<IpAddr, ScoreData>>;

/// 创建一个新的评分板
pub fn create_score_board() -> ScoreBoard {
    Arc::new(DashMap::new())
}
