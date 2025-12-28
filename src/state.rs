use crate::model::IpQuality;
use dashmap::DashMap;
use rand::prelude::*;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use anyhow::Result;

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

    pub fn get_best_ips(&self, target_colos: &[String], count: usize) -> Vec<IpAddr> {
        // Try to get from cache first
        {
            let cache = self.best_ips_cache.read().unwrap();
            if !cache.is_empty() {
                // If we have cached IPs, filter them by colo if needed
                let candidates: Vec<IpAddr> = if target_colos.is_empty() {
                    cache.clone()
                } else {
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
                    let actual_count = std::cmp::min(count, candidates.len());
                    return candidates.into_iter().take(actual_count).collect();
                }
            }
        }

        // Fallback to full scan if cache is empty or no matching colo found
        self.recalculate_best_ips_multiple(target_colos, count)
    }

    fn recalculate_best_ips_multiple(&self, target_colos: &[String], count: usize) -> Vec<IpAddr> {
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
            return Vec::new();
        }

        candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        let actual_count = std::cmp::min(count, candidates.len());
        candidates.into_iter().take(actual_count).map(|q| q.ip).collect()
    }

    pub fn get_all_ips(&self) -> Vec<IpQuality> {
        self.ips.iter().map(|entry| entry.value().clone()).collect()
    }

    pub fn save_to_file(&self, path: &str) -> Result<()> {
        let ips: Vec<IpQuality> = self.get_all_ips();
        let file = std::fs::File::create(path)?;
        let writer = std::io::BufWriter::new(file);
        serde_json::to_writer(writer, &ips)?;
        Ok(())
    }

    pub fn load_from_file(&self, path: &str) -> Result<()> {
        let file = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(file);
        let ips: Vec<IpQuality> = serde_json::from_reader(reader)?;
        
        self.ips.clear();
        for ip in ips {
            self.ips.insert(ip.ip, ip);
        }
        
        // Refresh cache immediately
        let mut all_ips: Vec<IpQuality> = self.ips.iter().map(|e| e.value().clone()).collect();
        all_ips.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        let top_ips: Vec<IpAddr> = all_ips.iter().take(100).map(|q| q.ip).collect();
        {
            let mut w = self.best_ips_cache.write().unwrap();
            *w = top_ips;
        }
        
        Ok(())
    }
}