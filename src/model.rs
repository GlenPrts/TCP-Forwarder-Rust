use std::net::IpAddr;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ipnet::IpNet;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubnetQuality {
    pub subnet: IpNet,
    pub score: f32,
    pub avg_latency: u128,
    pub avg_jitter: u128,
    pub avg_loss_rate: f32,
    pub sample_count: usize,
    pub colo: String,
    pub last_updated: DateTime<Utc>,
}

impl SubnetQuality {
    pub fn new(subnet: IpNet, samples: &[IpQuality]) -> Self {
        let sample_count = samples.len();
        if sample_count == 0 {
             return Self {
                subnet,
                score: 0.0,
                avg_latency: 0,
                avg_jitter: 0,
                avg_loss_rate: 0.0,
                sample_count: 0,
                colo: "UNKNOWN".to_string(),
                last_updated: Utc::now(),
            };
        }

        let avg_latency = samples.iter().map(|s| s.latency).sum::<u128>() / sample_count as u128;
        let avg_jitter = samples.iter().map(|s| s.jitter).sum::<u128>() / sample_count as u128;
        let avg_loss_rate = samples.iter().map(|s| s.loss_rate).sum::<f32>() / sample_count as f32;
        
        // Colo is determined by the most frequent colo in samples, or just the first valid one
        let colo = samples.first().map(|s| s.colo.clone()).unwrap_or_else(|| "UNKNOWN".to_string());

        // Score is the average of individual IP scores
        let avg_score = samples.iter().map(|s| s.score).sum::<f32>() / sample_count as f32;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpQuality {
    pub ip: IpAddr,
    pub latency: u128, // Average latency in milliseconds
    pub jitter: u128,  // Latency jitter in milliseconds
    pub loss_rate: f32, // Packet loss rate (0.0 - 1.0)
    pub score: f32,    // Calculated quality score (higher is better)
    pub colo: String,
    pub last_updated: DateTime<Utc>,
}

impl IpQuality {
    pub fn new(ip: IpAddr, latency: u128, jitter: u128, loss_rate: f32, colo: String) -> Self {
        let mut quality = Self {
            ip,
            latency,
            jitter,
            loss_rate,
            score: 0.0,
            colo,
            last_updated: Utc::now(),
        };
        quality.calculate_score();
        quality
    }

    pub fn calculate_score(&mut self) {
        // Score calculation algorithm:
        // Base score: 100
        // Latency penalty: -1 point per 10ms
        // Jitter penalty: -2 points per 10ms
        // Loss penalty: -100 points per 1% loss (loss is critical)

        let mut score = 100.0;
        let original_score = score;

        score -= (self.latency as f32) / 10.0;
        score -= (self.jitter as f32) / 5.0; // Jitter is worse than latency
        score -= self.loss_rate * 100.0 * 100.0; // 1% loss = 0.01 * 10000 = 100 points penalty

        tracing::debug!("IP {} score calculation: base={}, latency_penalty={}, jitter_penalty={}, loss_penalty={}, final={}",
            self.ip,
            original_score,
            (self.latency as f32) / 10.0,
            (self.jitter as f32) / 5.0,
            self.loss_rate * 100.0 * 100.0,
            score
        );

        if score < 0.0 {
            score = 0.0;
        }

        self.score = score;
    }
}