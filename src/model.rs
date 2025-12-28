use std::net::IpAddr;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
        
        score -= (self.latency as f32) / 10.0;
        score -= (self.jitter as f32) / 5.0; // Jitter is worse than latency
        score -= self.loss_rate * 100.0 * 100.0; // 1% loss = 0.01 * 10000 = 100 points penalty

        if score < 0.0 {
            score = 0.0;
        }
        
        self.score = score;
    }
}