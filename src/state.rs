use crate::model::IpQuality;
use chrono::{Duration, Utc};
use dashmap::DashMap;
use rand::prelude::*;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use tracing::info;

#[derive(Clone)]
pub struct IpManager {
    ips: Arc<DashMap<IpAddr, IpQuality>>,
    // Cache for best IPs to avoid sorting on every request
    // Stores a list of best IPs per colo (or global if no colo specified)
    // For simplicity, we just cache a global list of best IPs for now,
    // as filtering by colo is fast enough if the list is small,
    // but here we are optimizing the sorting of thousands of IPs.
    best_ips_cache: Arc<RwLock<Vec<IpAddr>>>,
}

impl IpManager {
    pub fn new() -> Self {
        Self {
            ips: Arc::new(DashMap::new()),
            best_ips_cache: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn update_ip(&self, quality: IpQuality) {
        self.ips.insert(quality.ip, quality);
    }

    pub fn get_best_ip(&self, target_colos: &[String]) -> Option<IpAddr> {
        // Try to get from cache first
        {
            let cache = self.best_ips_cache.read().unwrap();
            if !cache.is_empty() {
                // If we have cached IPs, filter them by colo if needed
                // Since cache is already sorted by score, we just pick one from top
                
                let candidates: Vec<IpAddr> = if target_colos.is_empty() {
                    cache.clone()
                } else {
                    // This might be slow if cache is huge, but cache should be top N only?
                    // Let's say cache holds top 100 IPs.
                    cache.iter()
                        .filter(|&ip| {
                            if let Some(entry) = self.ips.get(ip) {
                                target_colos.contains(&entry.value().colo)
                            } else {
                                false
                            }
                        })
                        .cloned()
                        .collect()
                };

                if !candidates.is_empty() {
                    let top_n = std::cmp::min(3, candidates.len());
                    let mut rng = rand::thread_rng();
                    let index = rng.gen_range(0..top_n);
                    return Some(candidates[index]);
                }
            }
        }

        // Fallback to full scan if cache is empty or no matching colo found
        // (This should happen rarely if cache is maintained properly)
        self.recalculate_best_ips(target_colos)
    }

    fn recalculate_best_ips(&self, target_colos: &[String]) -> Option<IpAddr> {
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

        candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

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
        let cache = self.best_ips_cache.clone();
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5)); // Refresh cache every 5s
            let mut cleanup_tick = 0;

            loop {
                interval.tick().await;
                
                // 1. Refresh Cache
                // We take all IPs, sort them, and keep top 100
                let mut all_ips: Vec<IpQuality> = ips.iter().map(|e| e.value().clone()).collect();
                all_ips.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
                
                let top_ips: Vec<IpAddr> = all_ips.iter().take(100).map(|q| q.ip).collect();
                
                {
                    let mut w = cache.write().unwrap();
                    *w = top_ips;
                }

                // 2. Cleanup Stale IPs (every 12 ticks = 60s)
                cleanup_tick += 1;
                if cleanup_tick >= 12 {
                    cleanup_tick = 0;
                    let now = Utc::now();
                    let threshold = now - Duration::minutes(10);

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
            }
        });
    }
}