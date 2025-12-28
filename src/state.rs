use crate::model::IpQuality;
use chrono::{Duration, Utc};
use dashmap::DashMap;
use rand::prelude::*;
use std::net::IpAddr;
use std::sync::Arc;
use tracing::info;

#[derive(Clone)]
pub struct IpManager {
    ips: Arc<DashMap<IpAddr, IpQuality>>,
}

impl IpManager {
    pub fn new() -> Self {
        Self {
            ips: Arc::new(DashMap::new()),
        }
    }

    pub fn update_ip(&self, quality: IpQuality) {
        self.ips.insert(quality.ip, quality);
    }

    pub fn get_best_ip(&self, target_colos: &[String]) -> Option<IpAddr> {
        let mut candidates: Vec<IpQuality> = self
            .ips
            .iter()
            .filter(|entry| {
                if target_colos.is_empty() {
                    true
                } else {
                    target_colos.contains(&entry.value().colo)
                }
            })
            .map(|entry| entry.value().clone())
            .collect();

        if candidates.is_empty() {
            return None;
        }

        // Sort by score (descending)
        candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        // Load balancing: pick random from top N (e.g., top 3 or top 10%)
        let top_n = std::cmp::min(3, candidates.len());
        let mut rng = rand::thread_rng();
        let index = rng.gen_range(0..top_n);

        Some(candidates[index].ip)
    }

    pub fn get_all_ips(&self) -> Vec<IpQuality> {
        self.ips.iter().map(|entry| entry.value().clone()).collect()
    }

    pub fn start_cleanup_task(&self) {
        let ips = self.ips.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                let now = Utc::now();
                let threshold = now - Duration::minutes(10); // Remove IPs older than 10 minutes

                let mut to_remove = Vec::new();
                for entry in ips.iter() {
                    if entry.value().last_updated < threshold {
                        to_remove.push(*entry.key());
                    }
                }

                for ip in to_remove {
                    ips.remove(&ip);
                    info!("Removed stale IP: {}", ip);
                }
            }
        });
    }
}