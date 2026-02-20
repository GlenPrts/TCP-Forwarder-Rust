use chrono::{DateTime, Utc};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use tracing::debug;

/// 评分计算常量
mod scoring {
    /// 基础分数
    pub const BASE_SCORE: f32 = 100.0;
    /// 延迟惩罚系数（每 10ms 扣 1 分）
    pub const LATENCY_PENALTY_PER_10MS: f32 = 1.0;
    /// 抖动惩罚系数（每 5ms 扣 1 分，比延迟更严重）
    pub const JITTER_PENALTY_PER_5MS: f32 = 1.0;
    /// 丢包惩罚系数（每 1% 丢包扣 50 分）
    pub const LOSS_PENALTY_PER_PERCENT: f32 = 50.0;
}

/// 子网质量数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubnetQuality {
    /// 子网
    pub subnet: IpNet,
    /// 综合评分（越高越好）
    pub score: f32,
    /// 平均延迟（毫秒）
    pub avg_latency: u64,
    /// 平均抖动（毫秒）
    pub avg_jitter: u64,
    /// 平均丢包率（0.0-1.0）
    pub avg_loss_rate: f32,
    /// 采样数量
    pub sample_count: usize,
    /// 数据中心代码
    pub colo: String,
    /// 最后更新时间
    pub last_updated: DateTime<Utc>,
}

impl SubnetQuality {
    /// 从 IP 质量样本创建子网质量数据
    pub fn new(subnet: IpNet, samples: &[IpQuality]) -> Self {
        if samples.is_empty() {
            return Self::empty(subnet);
        }

        let sample_count = samples.len();
        let count_u64 = sample_count as u64;
        let count_f32 = sample_count as f32;

        // 计算平均值
        let sum_latency: u64 = samples.iter().map(|s| s.latency).sum();
        let avg_latency = sum_latency.checked_div(count_u64).unwrap_or(0);

        let sum_jitter: u64 = samples.iter().map(|s| s.jitter).sum();
        let avg_jitter = sum_jitter.checked_div(count_u64).unwrap_or(0);

        let sum_loss: f32 = samples.iter().map(|s| s.loss_rate).sum();
        let avg_loss_rate = sum_loss / count_f32;

        // 确定主要的 colo（使用众数）
        let colo = determine_primary_colo(samples);

        // 计算平均评分
        let sum_score: f32 = samples.iter().map(|s| s.score).sum();
        let avg_score = sum_score / count_f32;

        Self {
            subnet,
            score: avg_score,
            avg_latency,
            avg_jitter,
            avg_loss_rate,
            sample_count,
            colo,
            last_updated: Utc::now(),
        }
    }

    /// 创建空的子网质量数据
    fn empty(subnet: IpNet) -> Self {
        Self {
            subnet,
            score: 0.0,
            avg_latency: 0,
            avg_jitter: 0,
            avg_loss_rate: 0.0,
            sample_count: 0,
            colo: "UNKNOWN".to_string(),
            last_updated: Utc::now(),
        }
    }
}

/// 确定主要的数据中心（使用众数）
fn determine_primary_colo(samples: &[IpQuality]) -> String {
    if samples.is_empty() {
        return "UNKNOWN".to_string();
    }

    let mut colo_counts: HashMap<&str, usize> = HashMap::new();
    for sample in samples {
        *colo_counts.entry(&sample.colo).or_insert(0) += 1;
    }

    colo_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(colo, _)| colo.to_string())
        .unwrap_or_else(|| "UNKNOWN".to_string())
}

/// 单个 IP 的质量数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpQuality {
    /// IP 地址
    pub ip: IpAddr,
    /// 平均延迟（毫秒）
    pub latency: u64,
    /// 延迟抖动（毫秒）
    pub jitter: u64,
    /// 丢包率（0.0-1.0）
    pub loss_rate: f32,
    /// 综合评分（越高越好）
    pub score: f32,
    /// 数据中心代码
    pub colo: String,
    /// 最后更新时间
    pub last_updated: DateTime<Utc>,
}

impl IpQuality {
    /// 创建新的 IP 质量数据
    pub fn new(
        ip: IpAddr,
        latency: u128,
        jitter: u128,
        loss_rate: f32,
        colo: String,
    ) -> Self {
        let mut quality = Self {
            ip,
            latency: latency as u64,
            jitter: jitter as u64,
            loss_rate,
            score: 0.0,
            colo,
            last_updated: Utc::now(),
        };
        quality.calculate_score();
        quality
    }

    /// 计算综合评分
    ///
    /// 评分算法：
    /// - 基础分：100
    /// - 延迟惩罚：每 10ms 扣 1 分
    /// - 抖动惩罚：每 5ms 扣 1 分（抖动比延迟更影响体验）
    /// - 丢包惩罚：每 1% 丢包扣 50 分（调整为更温和的惩罚，避免负分）
    pub fn calculate_score(&mut self) {
        use scoring::*;

        let latency_penalty =
            (self.latency as f32) / 10.0 * LATENCY_PENALTY_PER_10MS;
        let jitter_penalty =
            (self.jitter as f32) / 5.0 * JITTER_PENALTY_PER_5MS;
        // 调整丢包惩罚：每 1% 丢包扣 50 分
        let loss_penalty = self.loss_rate * 100.0 * LOSS_PENALTY_PER_PERCENT;

        let score =
            (BASE_SCORE - latency_penalty - jitter_penalty - loss_penalty)
                .max(0.0);

        debug!(
            "IP {} score: base={}, latency_p={:.2}, jitter_p={:.2}, \
             loss_p={:.2}, final={:.2}",
            self.ip,
            BASE_SCORE,
            latency_penalty,
            jitter_penalty,
            loss_penalty,
            score
        );

        self.score = score;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ip_quality_score_calculation() {
        // 完美的 IP：0 延迟，0 抖动，0 丢包
        let quality = IpQuality::new(
            "1.2.3.4".parse().unwrap(),
            0,
            0,
            0.0,
            "LAX".to_string(),
        );
        assert_eq!(quality.score, 100.0);

        // 100ms 延迟
        let quality = IpQuality::new(
            "1.2.3.4".parse().unwrap(),
            100,
            0,
            0.0,
            "LAX".to_string(),
        );
        assert_eq!(quality.score, 90.0); // 100 - 10

        // 1% 丢包（每 1% 扣 50 分）
        let quality = IpQuality::new(
            "1.2.3.4".parse().unwrap(),
            0,
            0,
            0.01,
            "LAX".to_string(),
        );
        assert_eq!(quality.score, 50.0); // 100 - 50 = 50
    }

    #[test]
    fn test_subnet_quality_from_samples() {
        let samples = vec![
            IpQuality::new(
                "1.2.3.1".parse().unwrap(),
                100,
                10,
                0.0,
                "LAX".to_string(),
            ),
            IpQuality::new(
                "1.2.3.2".parse().unwrap(),
                120,
                15,
                0.0,
                "LAX".to_string(),
            ),
        ];

        let subnet: IpNet = "1.2.3.0/24".parse().unwrap();
        let quality = SubnetQuality::new(subnet, &samples);

        assert_eq!(quality.sample_count, 2);
        assert_eq!(quality.avg_latency, 110); // (100 + 120) / 2
        assert_eq!(quality.colo, "LAX");
    }

    #[test]
    fn test_determine_primary_colo() {
        let samples = vec![
            IpQuality::new(
                "1.2.3.1".parse().unwrap(),
                100,
                10,
                0.0,
                "LAX".to_string(),
            ),
            IpQuality::new(
                "1.2.3.2".parse().unwrap(),
                100,
                10,
                0.0,
                "SJC".to_string(),
            ),
            IpQuality::new(
                "1.2.3.3".parse().unwrap(),
                100,
                10,
                0.0,
                "LAX".to_string(),
            ),
        ];

        let colo = determine_primary_colo(&samples);
        assert_eq!(colo, "LAX"); // LAX 出现 2 次，SJC 出现 1 次
    }

    #[test]
    fn test_empty_subnet_quality() {
        let subnet: IpNet = "1.2.3.0/24".parse().unwrap();
        let quality = SubnetQuality::new(subnet, &[]);

        assert_eq!(quality.sample_count, 0);
        assert_eq!(quality.score, 0.0);
        assert_eq!(quality.colo, "UNKNOWN");
    }
}
