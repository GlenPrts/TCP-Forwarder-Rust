use crate::config::AppConfig;
use crate::model::IpQuality;
use crate::state::IpManager;
use anyhow::Result;
use ipnet::IpNet;
use rand::prelude::*;
use reqwest::Client;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

const TIMEOUT_SECS: u64 = 1;
const MIN_CONCURRENCY: usize = 10;
const MAX_CONCURRENCY: usize = 200;
const PROBE_COUNT: usize = 5; // Number of probes per IP to calculate jitter/loss

pub async fn run_scan_once(
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    mut concurrency: usize,
    semaphore: Arc<Semaphore>
) -> usize {
    let mut cidrs = config.cidr_list.clone();
    
    match fetch_asn_cidrs(&config.asn_url).await {
        Ok(new_cidrs) => {
            if !new_cidrs.is_empty() {
                info!("Updated CIDR list with {} subnets from ASN", new_cidrs.len());
                cidrs = new_cidrs;
            }
        }
        Err(e) => {
            warn!("Failed to fetch ASN CIDRs, using cached list: {}", e);
        }
    }

    info!("Starting new scan round with concurrency: {}", concurrency);
    let generator = IpGenerator::new(cidrs.clone(), 24, 2);
    let ips = generator.generate();
    info!("Generated {} IPs to scan", ips.len());

    let mut tasks = Vec::new();
    let mut success_count = 0;
    let mut total_scanned = 0;

    for ip in ips {
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let ip_manager = ip_manager.clone();
        let config = config.clone();
        
        tasks.push(tokio::spawn(async move {
            let result = test_ip(ip, &config.trace_url).await;
            drop(permit);
            if let Some(quality) = result {
                ip_manager.update_ip(quality);
                return true;
            }
            false
        }));
    }

    for task in tasks {
        if let Ok(success) = task.await {
            total_scanned += 1;
            if success {
                success_count += 1;
            }
        }
    }

    if total_scanned > 0 {
        let success_rate = success_count as f64 / total_scanned as f64;
        info!("Round stats: {}/{} success (rate: {:.2}%)", success_count, total_scanned, success_rate * 100.0);

        if success_rate > 0.1 {
            concurrency = (concurrency + 10).min(MAX_CONCURRENCY);
        } else if success_rate < 0.01 {
            concurrency = (concurrency - 10).max(MIN_CONCURRENCY);
        }
    }
    
    concurrency
}

async fn fetch_asn_cidrs(url: &str) -> Result<Vec<IpNet>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    
    let response = client.get(url).send().await?.text().await?;
    let mut cidrs = Vec::new();
    
    for line in response.lines() {
        let line = line.trim();
        if !line.is_empty() {
            if let Ok(cidr) = line.parse::<IpNet>() {
                cidrs.push(cidr);
            }
        }
    }
    
    Ok(cidrs)
}

async fn test_ip(ip: IpAddr, trace_url: &str) -> Option<IpQuality> {
    // Extract hostname from trace_url to resolve
    let url = reqwest::Url::parse(trace_url).ok()?;
    let host = url.host_str()?;
    let port = url.port_or_known_default().unwrap_or(80);

    let client = Client::builder()
        .resolve(host, std::net::SocketAddr::new(ip, port))
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .ok()?;

    let mut latencies = Vec::new();
    let mut success_count = 0;
    let mut last_colo = String::new();

    for _ in 0..PROBE_COUNT {
        let start = Instant::now();
        match client.get(trace_url).send().await {
            Ok(response) => {
                let latency = start.elapsed().as_millis();
                if response.status().is_success() {
                    if let Ok(body) = response.text().await {
                        if let Some(colo) = parse_colo(&body) {
                            latencies.push(latency);
                            success_count += 1;
                            last_colo = colo;
                        }
                    }
                }
            }
            Err(_) => {}
        }
        // Small delay between probes
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    if success_count == 0 {
        return None;
    }

    let loss_rate = 1.0 - (success_count as f32 / PROBE_COUNT as f32);
    
    let avg_latency = if !latencies.is_empty() {
        latencies.iter().sum::<u128>() / latencies.len() as u128
    } else {
        0
    };

    let jitter = if latencies.len() > 1 {
        let mut sum_diff = 0;
        for i in 0..latencies.len() - 1 {
            sum_diff += (latencies[i] as i128 - latencies[i+1] as i128).abs();
        }
        (sum_diff / (latencies.len() - 1) as i128) as u128
    } else {
        0
    };

    debug!("Tested IP: {}, Colo: {}, Latency: {}ms, Jitter: {}ms, Loss: {:.0}%", 
           ip, last_colo, avg_latency, jitter, loss_rate * 100.0);

    Some(IpQuality::new(ip, avg_latency, jitter, loss_rate, last_colo))
}

fn parse_colo(body: &str) -> Option<String> {
    for line in body.lines() {
        if let Some(colo) = line.strip_prefix("colo=") {
            return Some(colo.to_string());
        }
    }
    None
}

struct IpGenerator {
    cidrs: Vec<IpNet>,
    subnet_mask: u8,
    samples_per_subnet: usize,
}

impl IpGenerator {
    fn new(cidrs: Vec<IpNet>, subnet_mask: u8, samples_per_subnet: usize) -> Self {
        Self {
            cidrs,
            subnet_mask,
            samples_per_subnet,
        }
    }

    fn generate(&self) -> Vec<IpAddr> {
        let mut ips = Vec::new();
        let mut rng = rand::thread_rng();

        for cidr in &self.cidrs {
            if let Some(subnets) = cidr.subnets(self.subnet_mask).ok() {
                for subnet in subnets {
                    for _ in 0..self.samples_per_subnet {
                        let ip = generate_random_ip_in_subnet(&subnet, &mut rng);
                        ips.push(ip);
                    }
                }
            } else {
                 for _ in 0..self.samples_per_subnet {
                    let ip = generate_random_ip_in_subnet(cidr, &mut rng);
                    ips.push(ip);
                }
            }
        }
        ips.shuffle(&mut rng);
        ips
    }
}

fn generate_random_ip_in_subnet(subnet: &IpNet, rng: &mut ThreadRng) -> IpAddr {
    match subnet {
        IpNet::V4(net) => {
            let start: u32 = net.network().into();
            let end: u32 = net.broadcast().into();
            let ip_u32 = rng.gen_range(start..=end);
            IpAddr::V4(std::net::Ipv4Addr::from(ip_u32))
        }
        IpNet::V6(net) => {
            let start: u128 = net.network().into();
            let end: u128 = net.broadcast().into();
            let ip_u128 = rng.gen_range(start..=end);
            IpAddr::V6(std::net::Ipv6Addr::from(ip_u128))
        }
    }
}