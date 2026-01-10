use crate::config::{AppConfig, ScanStrategyType};
use crate::model::{IpQuality, SubnetQuality};
use crate::state::IpManager;
use crate::utils::generate_random_ip_in_subnet;
use anyhow::Result;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use ipnet::IpNet;
use reqwest::Client;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, info, warn};

const TIMEOUT_SECS: u64 = 2;
const PROBE_COUNT: usize = 5;
const ASN_MAX_RETRIES: usize = 3;
const ASN_RETRY_DELAY_SECS: u64 = 2;
const FOCUSED_SCAN_SUBNET_MASK: u8 = 24;

#[derive(Debug, Default)]
pub struct ScanStats {
    pub total_scanned: usize,
    pub success_count: usize,
    pub updated_subnets: usize,
}

impl ScanStats {
    pub fn success_rate(&self) -> f64 {
        if self.total_scanned == 0 {
            0.0
        } else {
            self.success_count as f64 / self.total_scanned as f64
        }
    }
}

pub async fn run_scan_once(
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    semaphore: Arc<Semaphore>,
) -> ScanStats {
    let mut root_cidrs = config.cidr_list.clone();

    match fetch_asn_cidrs(&config.asn_url).await {
        Ok(new_cidrs) => {
            if !new_cidrs.is_empty() {
                info!(
                    "Updated CIDR list with {} subnets from ASN",
                    new_cidrs.len()
                );
                root_cidrs = new_cidrs;
            }
        }
        Err(e) => {
            warn!("Failed to fetch ASN CIDRs, using cached list: {}", e);
        }
    }

    let stats = match config.scan_strategy.r#type {
        ScanStrategyType::FullScan => {
            info!("Starting full scan...");
            run_full_scan(config, ip_manager, semaphore, &root_cidrs).await
        }
        ScanStrategyType::Adaptive => {
            info!("Starting adaptive scan...");
            run_adaptive_scan(config, ip_manager, semaphore, &root_cidrs).await
        }
    };

    info!(
        "Scan complete: {} IPs scanned, {} success (rate: {:.2}%), {} subnets updated",
        stats.total_scanned,
        stats.success_count,
        stats.success_rate() * 100.0,
        stats.updated_subnets
    );

    stats
}

async fn run_full_scan(
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    semaphore: Arc<Semaphore>,
    root_cidrs: &[IpNet],
) -> ScanStats {
    let target_subnets = split_cidrs_to_subnets(root_cidrs, FOCUSED_SCAN_SUBNET_MASK);
    info!("Total target subnets to scan: {}", target_subnets.len());

    let samples_per_subnet = config.scan_strategy.focused_samples_per_subnet;
    let mut targets = Vec::with_capacity(target_subnets.len() * samples_per_subnet);
    let mut rng = rand::thread_rng();
    for subnet in &target_subnets {
        for _ in 0..samples_per_subnet {
            let ip = generate_random_ip_in_subnet(subnet, &mut rng);
            targets.push((*subnet, ip));
        }
    }
    info!("Created {} scan targets", targets.len());

    let subnet_results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>> =
        Arc::new(Mutex::new(HashMap::with_capacity(target_subnets.len())));

    let concurrency_limit = semaphore.available_permits().max(50);
    info!("Scanning with concurrency limit: {}", concurrency_limit);

    let (success_count, total_scanned) = execute_scan_stream(
        targets,
        concurrency_limit,
        &config.trace_url,
        subnet_results.clone(),
    )
    .await;

    let results = subnet_results.lock().await;
    let mut updated_subnets = 0;

    for (subnet, samples) in results.iter() {
        if !samples.is_empty() {
            let quality = SubnetQuality::new(*subnet, samples);
            ip_manager.update_subnet(quality);
            updated_subnets += 1;
        }
    }

    ip_manager.recalculate_best_subnets(config.selection_top_k_percent);

    ScanStats {
        total_scanned,
        success_count,
        updated_subnets,
    }
}

async fn run_adaptive_scan(
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    semaphore: Arc<Semaphore>,
    root_cidrs: &[IpNet],
) -> ScanStats {
    let strategy = &config.scan_strategy;
    let concurrency_limit = semaphore.available_permits().max(50);

    // Phase 1: Wide & Sparse Scan
    info!("[Phase 1/3] Starting wide and sparse scan...");
    let initial_subnets = split_cidrs_to_subnets(root_cidrs, strategy.initial_scan_mask);
    info!(
        "[Phase 1/3] Split into {} /{} subnets for initial scan.",
        initial_subnets.len(),
        strategy.initial_scan_mask
    );

    let mut initial_targets =
        Vec::with_capacity(initial_subnets.len() * strategy.initial_samples_per_subnet);
    let mut rng = rand::thread_rng();
    for subnet in &initial_subnets {
        for _ in 0..strategy.initial_samples_per_subnet {
            let ip = generate_random_ip_in_subnet(subnet, &mut rng);
            initial_targets.push((*subnet, ip));
        }
    }
    info!(
        "[Phase 1/3] Created {} scan targets.",
        initial_targets.len()
    );

    let initial_results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>> =
        Arc::new(Mutex::new(HashMap::with_capacity(initial_subnets.len())));

    let (initial_success, initial_total) = execute_scan_stream(
        initial_targets,
        concurrency_limit,
        &config.trace_url,
        initial_results.clone(),
    )
    .await;

    info!(
        "[Phase 1/3] Completed: {}/{} IPs successful.",
        initial_success, initial_total
    );

    // Phase 2: Hot Spot Analysis
    info!("[Phase 2/3] Starting hot spot analysis...");
    let initial_scan_results = initial_results.lock().await;
    let mut promising_subnets: Vec<SubnetQuality> = initial_scan_results
        .iter()
        .filter_map(|(subnet, samples)| {
            if samples.is_empty() {
                None
            } else {
                Some(SubnetQuality::new(*subnet, samples))
            }
        })
        .collect();

    promising_subnets.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());

    let top_k_count =
        (promising_subnets.len() as f64 * strategy.promising_subnet_percent).ceil() as usize;
    let hot_spots: Vec<IpNet> = promising_subnets
        .into_iter()
        .take(top_k_count)
        .map(|q| q.subnet)
        .collect();

    info!(
        "[Phase 2/3] Identified {} hot spots (top {}%) for focused scan.",
        hot_spots.len(),
        strategy.promising_subnet_percent * 100.0
    );

    if hot_spots.is_empty() {
        warn!("No promising subnets found in phase 1. Aborting scan.");
        return ScanStats {
            total_scanned: initial_total,
            success_count: initial_success,
            updated_subnets: 0,
        };
    }

    // Phase 3: Focused Scan
    info!("[Phase 3/3] Starting focused scan on hot spots...");
    let focused_subnets = split_cidrs_to_subnets(&hot_spots, FOCUSED_SCAN_SUBNET_MASK);
    info!(
        "[Phase 3/3] Split hot spots into {} /{} subnets for focused scan.",
        focused_subnets.len(),
        FOCUSED_SCAN_SUBNET_MASK
    );

    let mut focused_targets =
        Vec::with_capacity(focused_subnets.len() * strategy.focused_samples_per_subnet);
    for subnet in &focused_subnets {
        for _ in 0..strategy.focused_samples_per_subnet {
            let ip = generate_random_ip_in_subnet(subnet, &mut rng);
            focused_targets.push((*subnet, ip));
        }
    }
    info!(
        "[Phase 3/3] Created {} scan targets.",
        focused_targets.len()
    );

    let focused_results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>> =
        Arc::new(Mutex::new(HashMap::with_capacity(focused_subnets.len())));

    let (focused_success, focused_total) = execute_scan_stream(
        focused_targets,
        concurrency_limit,
        &config.trace_url,
        focused_results.clone(),
    )
    .await;

    info!(
        "[Phase 3/3] Completed: {}/{} IPs successful.",
        focused_success, focused_total
    );

    // Phase 4: Aggregation & Persistence
    let final_results = focused_results.lock().await;
    let mut updated_subnets = 0;
    for (subnet, samples) in final_results.iter() {
        if !samples.is_empty() {
            let quality = SubnetQuality::new(*subnet, samples);
            ip_manager.update_subnet(quality);
            updated_subnets += 1;
        }
    }

    ip_manager.recalculate_best_subnets(config.selection_top_k_percent);

    ScanStats {
        total_scanned: initial_total + focused_total,
        success_count: initial_success + focused_success,
        updated_subnets,
    }
}

async fn execute_scan_stream(
    targets: Vec<(IpNet, IpAddr)>,
    concurrency_limit: usize,
    trace_url: &str,
    results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>>,
) -> (usize, usize) {
    let mut success_count = 0;
    let mut total_scanned = 0;

    let pb = ProgressBar::new(targets.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
            .unwrap()
            .progress_chars("#>-"),
    );
    pb.set_message("Scanning IPs...");

    let trace_url = Arc::new(trace_url.to_string());

    let mut stream = stream::iter(targets)
        .map(|(subnet, ip)| {
            let trace_url = trace_url.clone();
            async move {
                let quality = test_ip(ip, &trace_url).await;
                (subnet, quality)
            }
        })
        .buffer_unordered(concurrency_limit);

    while let Some((subnet, quality_opt)) = stream.next().await {
        total_scanned += 1;
        if let Some(quality) = quality_opt {
            success_count += 1;
            pb.set_message(format!("Success: {}", success_count));

            let mut guard = results.lock().await;
            guard.entry(subnet).or_insert_with(Vec::new).push(quality);
        }
        pb.inc(1);
    }

    pb.finish_with_message(format!("Scan complete. Found {} valid IPs.", success_count));

    (success_count, total_scanned)
}

fn split_cidrs_to_subnets(cidrs: &[IpNet], mask: u8) -> Vec<IpNet> {
    let mut target_subnets = Vec::new();
    for cidr in cidrs {
        match cidr.subnets(mask) {
            Ok(subnets) => target_subnets.extend(subnets),
            Err(_) => target_subnets.push(*cidr),
        }
    }
    target_subnets
}

async fn fetch_asn_cidrs(url: &str) -> Result<Vec<IpNet>> {
    let client = Client::builder().timeout(Duration::from_secs(15)).build()?;

    let mut last_error = None;

    for attempt in 1..=ASN_MAX_RETRIES {
        match client.get(url).send().await {
            Ok(resp) => match resp.text().await {
                Ok(text) => {
                    return Ok(parse_cidr_list(&text));
                }
                Err(e) => {
                    warn!(
                        "Failed to read ASN response (attempt {}/{}): {}",
                        attempt, ASN_MAX_RETRIES, e
                    );
                    last_error = Some(e.into());
                }
            },
            Err(e) => {
                warn!(
                    "Failed to fetch ASN data (attempt {}/{}): {}",
                    attempt, ASN_MAX_RETRIES, e
                );
                last_error = Some(e.into());
            }
        }

        if attempt < ASN_MAX_RETRIES {
            tokio::time::sleep(Duration::from_secs(ASN_RETRY_DELAY_SECS)).await;
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Unknown error fetching ASN CIDRs")))
}

fn parse_cidr_list(text: &str) -> Vec<IpNet> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                None
            } else {
                line.parse::<IpNet>().ok()
            }
        })
        .collect()
}

async fn test_ip(ip: IpAddr, trace_url: &str) -> Option<IpQuality> {
    let url = reqwest::Url::parse(trace_url).ok()?;
    let host = url.host_str()?;
    let port = url.port_or_known_default().unwrap_or(80);

    let client = Client::builder()
        .resolve(host, std::net::SocketAddr::new(ip, port))
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(1))
        .build()
        .ok()?;

    let mut latencies = Vec::with_capacity(PROBE_COUNT);
    let mut success_count = 0;
    let mut last_colo = String::new();

    for probe_idx in 0..PROBE_COUNT {
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
            Err(e) => {
                debug!("Probe {} for IP {} failed: {}", probe_idx + 1, ip, e);
            }
        }
        if probe_idx < PROBE_COUNT - 1 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    if success_count == 0 {
        return None;
    }

    let loss_rate = 1.0 - (success_count as f32 / PROBE_COUNT as f32);
    let avg_latency = calculate_average(&latencies);
    let jitter = calculate_jitter(&latencies);

    debug!(
        "Tested IP: {}, Colo: {}, Latency: {}ms, Jitter: {}ms, Loss: {:.0}%",
        ip,
        last_colo,
        avg_latency,
        jitter,
        loss_rate * 100.0
    );

    Some(IpQuality::new(
        ip,
        avg_latency,
        jitter,
        loss_rate,
        last_colo,
    ))
}

fn calculate_average(values: &[u128]) -> u128 {
    if values.is_empty() {
        0
    } else {
        values.iter().sum::<u128>() / values.len() as u128
    }
}

fn calculate_jitter(latencies: &[u128]) -> u128 {
    if latencies.len() < 2 {
        return 0;
    }

    let sum_diff: i128 = latencies
        .windows(2)
        .map(|w| (w[0] as i128 - w[1] as i128).abs())
        .sum();

    (sum_diff / (latencies.len() - 1) as i128) as u128
}

fn parse_colo(body: &str) -> Option<String> {
    body.lines()
        .find_map(|line| line.strip_prefix("colo=").map(|s| s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_colo() {
        let body = "fl=123\ncolo=LAX\nip=1.2.3.4";
        assert_eq!(parse_colo(body), Some("LAX".to_string()));

        let body_no_colo = "fl=123\nip=1.2.3.4";
        assert_eq!(parse_colo(body_no_colo), None);
    }

    #[test]
    fn test_parse_cidr_list() {
        let text = "104.16.0.0/12\n# comment\n172.64.0.0/13\n\n";
        let cidrs = parse_cidr_list(text);
        assert_eq!(cidrs.len(), 2);
    }

    #[test]
    fn test_calculate_jitter() {
        let latencies = vec![100, 110, 105, 115];
        let jitter = calculate_jitter(&latencies);
        assert_eq!(jitter, 8);
    }

    #[test]
    fn test_calculate_average() {
        let values = vec![100, 200, 300];
        assert_eq!(calculate_average(&values), 200);
        assert_eq!(calculate_average(&[]), 0);
    }
}
