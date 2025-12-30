use crate::model::SubnetQuality;
use dashmap::DashMap;
use ipnet::IpNet;
use rand::prelude::*;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use anyhow::Result;

#[derive(Clone)]
pub struct IpManager {
    // Store SubnetQuality keyed by Subnet
    subnets: Arc<DashMap<IpNet, SubnetQuality>>,
    // Cache for best subnets (top K%)
    best_subnets_cache: Arc<RwLock<Vec<IpNet>>>,
}

impl IpManager {
    pub fn new() -> Self {
        Self {
            subnets: Arc::new(DashMap::new()),
            best_subnets_cache: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn update_subnet(&self, quality: SubnetQuality) {
        self.subnets.insert(quality.subnet, quality);
    }

    pub fn recalculate_best_subnets(&self, top_k_percent: f64) {
        let mut all_subnets: Vec<SubnetQuality> = self.subnets.iter().map(|e| e.value().clone()).collect();
        
        if all_subnets.is_empty() {
            let mut cache = self.best_subnets_cache.write().unwrap();
            cache.clear();
            return;
        }

        // Sort by score descending
        all_subnets.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        let total = all_subnets.len();
        let k = (total as f64 * top_k_percent).ceil() as usize;
        let k = std::cmp::max(1, k); // At least 1
        let k = std::cmp::min(k, total);

        let top_subnets: Vec<IpNet> = all_subnets.iter().take(k).map(|q| q.subnet).collect();

        {
            let mut cache = self.best_subnets_cache.write().unwrap();
            *cache = top_subnets;
        }
    }

    // New main interface for getting target IPs for forwarding
    // 1. Filter subnets by colo (if needed) from the cached "best subnets"
    // 2. Randomly select n subnets from the filtered best subnets
    // 3. For each selected subnet, randomly generate m IPs
    pub fn get_target_ips(&self, target_colos: &[String], n_subnets: usize, m_ips: usize) -> Vec<IpAddr> {
        let best_subnets = self.best_subnets_cache.read().unwrap();
        
        if best_subnets.is_empty() {
            // If cache is empty, we might need to fallback or trigger recalculation, 
            // but normally scanner loop will keep it updated.
            // For now return empty, server will handle it.
            return Vec::new();
        }

        // Filter by colo
        let candidates: Vec<IpNet> = if target_colos.is_empty() {
            best_subnets.clone()
        } else {
             best_subnets.iter()
                .filter(|&subnet| {
                    if let Some(entry) = self.subnets.get(subnet) {
                        target_colos.contains(&entry.value().colo)
                    } else {
                        false
                    }
                })
                .cloned()
                .collect()
        };

        if candidates.is_empty() {
            return Vec::new();
        }

        let mut rng = rand::thread_rng();
        
        // Randomly select n subnets
        let selected_subnets: Vec<IpNet> = if candidates.len() <= n_subnets {
            candidates
        } else {
            candidates.choose_multiple(&mut rng, n_subnets).cloned().collect()
        };

        // For each subnet, generate m IPs
        let mut target_ips = Vec::new();
        for subnet in selected_subnets {
            for _ in 0..m_ips {
                target_ips.push(generate_random_ip_in_subnet(&subnet, &mut rng));
            }
        }

        target_ips
    }

    pub fn get_all_subnets(&self) -> Vec<SubnetQuality> {
        self.subnets.iter().map(|entry| entry.value().clone()).collect()
    }

    pub fn save_to_file(&self, path: &str) -> Result<()> {
        let subnets: Vec<SubnetQuality> = self.get_all_subnets();
        let file = std::fs::File::create(path)?;
        let writer = std::io::BufWriter::new(file);
        serde_json::to_writer(writer, &subnets)?;
        Ok(())
    }

    pub fn load_from_file(&self, path: &str, top_k_percent: f64) -> Result<()> {
        let file = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(file);
        let subnets: Vec<SubnetQuality> = serde_json::from_reader(reader)?;
        
        self.subnets.clear();
        for subnet in subnets {
            self.subnets.insert(subnet.subnet, subnet);
        }
        
        // Refresh cache immediately
        self.recalculate_best_subnets(top_k_percent);
        
        Ok(())
    }
}

fn generate_random_ip_in_subnet(subnet: &IpNet, rng: &mut ThreadRng) -> IpAddr {
    match subnet {
        IpNet::V4(net) => {
            let start: u32 = net.network().into();
            let end: u32 = net.broadcast().into();
            // Avoid network and broadcast addresses if possible, though strict /32 logic applies
            let start = start + 1; 
            let end = end - 1;
            
            if start > end {
                // Should not happen for valid /24
                 IpAddr::V4(std::net::Ipv4Addr::from(start - 1))
            } else {
                let ip_u32 = rng.gen_range(start..=end);
                IpAddr::V4(std::net::Ipv4Addr::from(ip_u32))
            }
        }
        IpNet::V6(net) => {
            let start: u128 = net.network().into();
            let end: u128 = net.broadcast().into();
            let ip_u128 = rng.gen_range(start..=end);
            IpAddr::V6(std::net::Ipv6Addr::from(ip_u128))
        }
    }
}