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
use tokio::sync::Mutex;
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

/// 执行一次 IP 扫描
///
/// # 参数
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
/// - `concurrency`: 并发扫描数
///
/// # 返回值
/// 扫描统计信息
pub async fn run_scan_once(
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    concurrency: usize,
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
            run_full_scan(config, ip_manager, concurrency, &root_cidrs).await
        }
        ScanStrategyType::Adaptive => {
            info!("Starting adaptive scan...");
            let cfg = config;
            run_adaptive_scan(cfg, ip_manager, concurrency, &root_cidrs).await
        }
    };

    info!(
        "Scan complete: {} scanned, {} success ({:.2}%), {} subnets",
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
    concurrency: usize,
    root_cidrs: &[IpNet],
) -> ScanStats {
    let mask = FOCUSED_SCAN_SUBNET_MASK;
    let target_subnets = split_cidrs_to_subnets(root_cidrs, mask);
    info!("Total target subnets to scan: {}", target_subnets.len());

    let samples = config.scan_strategy.focused_samples_per_subnet;
    let capacity = target_subnets.len() * samples;
    let mut targets = Vec::with_capacity(capacity);
    let mut rng = rand::thread_rng();
    for subnet in &target_subnets {
        for _ in 0..samples {
            let ip = generate_random_ip_in_subnet(subnet, &mut rng);
            targets.push((*subnet, ip));
        }
    }
    info!("Created {} scan targets", targets.len());

    let subnet_results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>> =
        Arc::new(Mutex::new(HashMap::with_capacity(target_subnets.len())));

    let concurrency_limit = concurrency.max(50);
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
    concurrency: usize,
    root_cidrs: &[IpNet],
) -> ScanStats {
    let concurrency_limit = concurrency.max(50);

    let (initial_success, initial_total, initial_results) =
        run_initial_scan(&config, concurrency_limit, root_cidrs).await;

    let hot_spots = analyze_hot_spots(
        initial_results,
        config.scan_strategy.promising_subnet_percent,
    )
    .await;

    if hot_spots.is_empty() {
        warn!("No promising subnets found in phase 1. Aborting scan.");
        return ScanStats {
            total_scanned: initial_total,
            success_count: initial_success,
            updated_subnets: 0,
        };
    }

    let (focused_success, focused_total, focused_results) =
        run_focused_scan(&config, concurrency_limit, &hot_spots).await;

    let top_k = config.selection_top_k_percent;
    let mgr = &ip_manager;
    let updated_subnets = aggregate_and_persist(focused_results, mgr, top_k).await;

    ScanStats {
        total_scanned: initial_total + focused_total,
        success_count: initial_success + focused_success,
        updated_subnets,
    }
}

/// 第一阶段：广域稀疏扫描
///
/// # 参数
/// - `config`: 应用配置
/// - `concurrency_limit`: 并发限制
/// - `root_cidrs`: 根 CIDR 列表
///
/// # 返回值
/// (成功数, 总数, 扫描结果)
async fn run_initial_scan(
    config: &AppConfig,
    concurrency_limit: usize,
    root_cidrs: &[IpNet],
) -> (usize, usize, Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>>) {
    let strategy = &config.scan_strategy;

    info!("[Phase 1/3] Starting wide and sparse scan...");
    let mask = strategy.initial_scan_mask;
    let subnets = split_cidrs_to_subnets(root_cidrs, mask);
    info!(
        "[Phase 1/3] Split into {} /{} subnets.",
        subnets.len(),
        mask
    );

    let samples = strategy.initial_samples_per_subnet;
    let targets = generate_scan_targets(&subnets, samples);
    info!("[Phase 1/3] Created {} scan targets.", targets.len());

    let results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>> =
        Arc::new(Mutex::new(HashMap::with_capacity(subnets.len())));

    let (success, total) = execute_scan_stream(
        targets,
        concurrency_limit,
        &config.trace_url,
        results.clone(),
    )
    .await;

    info!(
        "[Phase 1/3] Completed: {}/{} IPs successful.",
        success, total
    );

    (success, total, results)
}

/// 第二阶段：热点分析
///
/// # 参数
/// - `initial_results`: 初始扫描结果
/// - `promising_percent`: 热点筛选比例
///
/// # 返回值
/// 热点子网列表
async fn analyze_hot_spots(
    initial_results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>>,
    promising_percent: f64,
) -> Vec<IpNet> {
    info!("[Phase 2/3] Starting hot spot analysis...");

    let results = initial_results.lock().await;
    let mut promising: Vec<SubnetQuality> = results
        .iter()
        .filter_map(|(subnet, samples)| {
            if samples.is_empty() {
                return None;
            }
            Some(SubnetQuality::new(*subnet, samples))
        })
        .collect();

    promising.sort_by(|a, b| b.score.total_cmp(&a.score));

    let top_k = (promising.len() as f64 * promising_percent).ceil() as usize;
    let hot_spots: Vec<IpNet> = promising
        .into_iter()
        .take(top_k)
        .map(|q| q.subnet)
        .collect();

    info!(
        "[Phase 2/3] Identified {} hot spots (top {}%).",
        hot_spots.len(),
        promising_percent * 100.0
    );

    hot_spots
}

/// 第三阶段：精细扫描
///
/// # 参数
/// - `config`: 应用配置
/// - `concurrency_limit`: 并发限制
/// - `hot_spots`: 热点子网列表
///
/// # 返回值
/// (成功数, 总数, 扫描结果)
async fn run_focused_scan(
    config: &AppConfig,
    concurrency_limit: usize,
    hot_spots: &[IpNet],
) -> (usize, usize, Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>>) {
    let strategy = &config.scan_strategy;

    info!("[Phase 3/3] Starting focused scan on hot spots...");
    let subnets = split_cidrs_to_subnets(hot_spots, FOCUSED_SCAN_SUBNET_MASK);
    info!(
        "[Phase 3/3] Split into {} /{} subnets.",
        subnets.len(),
        FOCUSED_SCAN_SUBNET_MASK
    );

    let samples = strategy.focused_samples_per_subnet;
    let targets = generate_scan_targets(&subnets, samples);
    info!("[Phase 3/3] Created {} scan targets.", targets.len());

    let results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>> =
        Arc::new(Mutex::new(HashMap::with_capacity(subnets.len())));

    let (success, total) = execute_scan_stream(
        targets,
        concurrency_limit,
        &config.trace_url,
        results.clone(),
    )
    .await;

    info!(
        "[Phase 3/3] Completed: {}/{} IPs successful.",
        success, total
    );

    (success, total, results)
}

/// 第四阶段：聚合与持久化
///
/// # 参数
/// - `results`: 扫描结果
/// - `ip_manager`: IP 管理器
/// - `top_k_percent`: 最佳子网筛选比例
///
/// # 返回值
/// 更新的子网数量
async fn aggregate_and_persist(
    results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>>,
    ip_manager: &IpManager,
    top_k_percent: f64,
) -> usize {
    let guard = results.lock().await;
    let mut updated = 0;

    for (subnet, samples) in guard.iter() {
        if samples.is_empty() {
            continue;
        }
        let quality = SubnetQuality::new(*subnet, samples);
        ip_manager.update_subnet(quality);
        updated += 1;
    }

    ip_manager.recalculate_best_subnets(top_k_percent);
    updated
}

/// 生成扫描目标
///
/// # 参数
/// - `subnets`: 子网列表
/// - `samples`: 每个子网的采样数
///
/// # 返回值
/// (子网, IP) 元组列表
fn generate_scan_targets(subnets: &[IpNet], samples: usize) -> Vec<(IpNet, IpAddr)> {
    let mut targets = Vec::with_capacity(subnets.len() * samples);
    let mut rng = rand::thread_rng();

    for subnet in subnets {
        for _ in 0..samples {
            let ip = generate_random_ip_in_subnet(subnet, &mut rng);
            targets.push((*subnet, ip));
        }
    }

    targets
}

async fn execute_scan_stream(
    targets: Vec<(IpNet, IpAddr)>,
    concurrency_limit: usize,
    trace_url: &str,
    results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>>,
) -> (usize, usize) {
    let mut success_count = 0;
    let mut total_scanned = 0;

    // 如果没有目标，直接返回
    if targets.is_empty() {
        return (0, 0);
    }

    let pb = ProgressBar::new(targets.len() as u64);
    let tpl = "{spinner:.green} [{elapsed_precise}] \
        [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}";
    pb.set_style(
        ProgressStyle::default_bar()
            .template(tpl)
            .unwrap()
            .progress_chars("#>-"),
    );
    pb.set_message("Scanning IPs...");

    let trace_url = Arc::new(trace_url.to_string());

    // 使用流式处理，避免一次性加载所有目标到内存
    let mut stream = stream::iter(targets)
        .map(|(subnet, ip)| {
            let trace_url = trace_url.clone();
            async move {
                let quality = test_ip(ip, &trace_url).await;
                (subnet, quality)
            }
        })
        .buffer_unordered(concurrency_limit);

    // 批量处理结果，减少锁竞争
    let mut batch: Vec<(IpNet, IpQuality)> = Vec::with_capacity(100);
    const BATCH_SIZE: usize = 100;

    while let Some((subnet, quality_opt)) = stream.next().await {
        total_scanned += 1;
        if let Some(quality) = quality_opt {
            success_count += 1;
            pb.set_message(format!("Success: {}", success_count));
            batch.push((subnet, quality));

            // 批量写入，减少锁竞争
            if batch.len() >= BATCH_SIZE {
                let mut guard = results.lock().await;
                for (subnet, quality) in batch.drain(..) {
                    guard.entry(subnet).or_insert_with(Vec::new).push(quality);
                }
            }
        }
        pb.inc(1);
    }

    // 处理剩余的批次
    if !batch.is_empty() {
        let mut guard = results.lock().await;
        for (subnet, quality) in batch {
            guard.entry(subnet).or_insert_with(Vec::new).push(quality);
        }
    }

    let msg = format!("Scan complete. Found {} valid IPs.", success_count);
    pb.finish_with_message(msg);

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

    let err = last_error.unwrap_or_else(|| anyhow::anyhow!("Unknown error fetching ASN CIDRs"));
    Err(err)
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
        return 0;
    }
    let sum = values.iter().fold(0u128, |acc, &v| acc.saturating_add(v));
    sum / values.len() as u128
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
