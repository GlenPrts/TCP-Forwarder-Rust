use crate::config::{AppConfig, ScanStrategyType};
use crate::model::{IpQuality, SubnetQuality};
use crate::state::IpManager;
use crate::utils::generate_random_ip_in_subnet;
use anyhow::Result;
use async_stream::stream;
use chrono::Utc;
use futures::stream::{Stream, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use ipnet::IpNet;
use parking_lot::Mutex;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rayon::prelude::*;
use reqwest::Client;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

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
            return 0.0;
        }
        self.success_count as f64 / self.total_scanned as f64
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

/// 执行全量扫描
///
/// # 参数
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
/// - `concurrency`: 并发数
/// - `root_cidrs`: 根 CIDR 列表
///
/// # 返回值
/// 扫描统计信息
async fn run_full_scan(
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    concurrency: usize,
    root_cidrs: &[IpNet],
) -> ScanStats {
    let mask = FOCUSED_SCAN_SUBNET_MASK;
    let root_cidrs_vec = root_cidrs.to_vec();
    let target_subnets =
        tokio::task::spawn_blocking(move || split_cidrs_to_subnets(root_cidrs_vec, mask))
            .await
            .unwrap_or_default();
    info!("Total target subnets to scan: {}", target_subnets.len());

    let samples = config.scan_strategy.focused_samples_per_subnet;
    let total_count = target_subnets.len() * samples;
    let targets = generate_scan_targets_stream(target_subnets, samples);
    info!("Created {} scan targets", total_count);

    let subnet_results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>> =
        Arc::new(Mutex::new(HashMap::with_capacity(100))); // 初始容量设为 100

    // 保证最低并发数为 50
    let effective_concurrency = concurrency.max(50);
    info!("Scanning with concurrency limit: {}", effective_concurrency);

    let (success_count, total_scanned) = execute_scan_stream(
        Box::pin(targets),
        total_count,
        effective_concurrency,
        &config.trace_url,
        subnet_results.clone(),
        &ip_manager,
    )
    .await;

    let manager = ip_manager.clone();
    let cfg = config.clone();
    let top_k = config.selection_top_k_percent;
    let updated_subnets = tokio::task::spawn_blocking(move || {
        aggregate_and_persist(subnet_results, &manager, &cfg, top_k)
    })
    .await
    .unwrap_or(0);

    ScanStats {
        total_scanned,
        success_count,
        updated_subnets,
    }
}

/// 执行自适应扫描
///
/// # 参数
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
/// - `concurrency`: 并发数
/// - `root_cidrs`: 根 CIDR 列表
///
/// # 返回值
/// 扫描统计信息
async fn run_adaptive_scan(
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    concurrency: usize,
    root_cidrs: &[IpNet],
) -> ScanStats {
    // 保证最低并发数为 50
    let effective_concurrency = concurrency.max(50);

    let (initial_success, initial_total, initial_results) =
        run_initial_scan(&config, &ip_manager, effective_concurrency, root_cidrs).await;

    let promising_percent = config.scan_strategy.promising_subnet_percent;
    let hot_spots =
        tokio::task::spawn_blocking(move || analyze_hot_spots(initial_results, promising_percent))
            .await
            .unwrap_or_default();

    if hot_spots.is_empty() {
        warn!("No promising subnets found in phase 1. Aborting scan.");
        return ScanStats {
            total_scanned: initial_total,
            success_count: initial_success,
            updated_subnets: 0,
        };
    }

    let (focused_success, focused_total, focused_results) =
        run_focused_scan(&config, &ip_manager, effective_concurrency, &hot_spots).await;

    let top_k = config.selection_top_k_percent;
    let manager = ip_manager.clone();
    let cfg = config.clone();
    let updated_subnets = tokio::task::spawn_blocking(move || {
        aggregate_and_persist(focused_results, &manager, &cfg, top_k)
    })
    .await
    .unwrap_or(0);

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
    ip_manager: &IpManager,
    concurrency_limit: usize,
    root_cidrs: &[IpNet],
) -> (usize, usize, Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>>) {
    let strategy = &config.scan_strategy;

    info!("[Phase 1/3] Starting wide and sparse scan...");
    let mask = strategy.initial_scan_mask;
    let root_cidrs_vec = root_cidrs.to_vec();
    let subnets = tokio::task::spawn_blocking(move || split_cidrs_to_subnets(root_cidrs_vec, mask))
        .await
        .unwrap_or_default();
    info!(
        "[Phase 1/3] Split into {} /{} subnets.",
        subnets.len(),
        mask
    );

    let samples = strategy.initial_samples_per_subnet;
    let total_count = subnets.len() * samples;
    let targets = generate_scan_targets_stream(subnets, samples);
    info!("[Phase 1/3] Created {} scan targets.", total_count);

    let results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>> =
        Arc::new(Mutex::new(HashMap::with_capacity(100)));

    let (success, total) = execute_scan_stream(
        Box::pin(targets),
        total_count,
        concurrency_limit,
        &config.trace_url,
        results.clone(),
        ip_manager,
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
fn analyze_hot_spots(
    initial_results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>>,
    promising_percent: f64,
) -> Vec<IpNet> {
    info!("[Phase 2/3] Starting hot spot analysis...");

    let results = initial_results.lock();
    let mut promising: Vec<SubnetQuality> = results
        .par_iter()
        .filter_map(|(subnet, samples)| {
            if samples.is_empty() {
                return None;
            }
            Some(SubnetQuality::new(*subnet, samples))
        })
        .collect();

    promising.par_sort_by(|a, b| b.score.total_cmp(&a.score));

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
    ip_manager: &IpManager,
    concurrency_limit: usize,
    hot_spots: &[IpNet],
) -> (usize, usize, Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>>) {
    let strategy = &config.scan_strategy;

    info!("[Phase 3/3] Starting focused scan on hot spots...");
    let hot_spots_vec = hot_spots.to_vec();
    let subnets = tokio::task::spawn_blocking(move || {
        split_cidrs_to_subnets(hot_spots_vec, FOCUSED_SCAN_SUBNET_MASK)
    })
    .await
    .unwrap_or_default();
    info!(
        "[Phase 3/3] Split into {} /{} subnets.",
        subnets.len(),
        FOCUSED_SCAN_SUBNET_MASK
    );

    let samples = strategy.focused_samples_per_subnet;
    let total_count = subnets.len() * samples;
    let targets = generate_scan_targets_stream(subnets, samples);
    info!("[Phase 3/3] Created {} scan targets.", total_count);

    let results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>> =
        Arc::new(Mutex::new(HashMap::with_capacity(100)));

    let (success, total) = execute_scan_stream(
        Box::pin(targets),
        total_count,
        concurrency_limit,
        &config.trace_url,
        results.clone(),
        ip_manager,
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
fn aggregate_and_persist(
    results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>>,
    ip_manager: &IpManager,
    config: &AppConfig,
    top_k_percent: f64,
) -> usize {
    let guard = results.lock();

    guard.par_iter().for_each(|(subnet, samples)| {
        if !samples.is_empty() {
            let quality = SubnetQuality::new(*subnet, samples);
            ip_manager.update_subnet(quality);
        }
    });

    let updated = guard.len();
    drop(guard); // 提前释放锁

    let now = Utc::now();
    let (removed_expired, removed_evicted) =
        ip_manager.cleanup_subnets(now, config.subnet_ttl_secs, config.max_subnets);
    if removed_expired > 0 || removed_evicted > 0 {
        info!(
            "Cleaned subnets: expired={}, evicted={}, remaining={}",
            removed_expired,
            removed_evicted,
            ip_manager.subnet_count(),
        );
    }

    ip_manager.recalculate_best_subnets(top_k_percent);
    updated
}

/// 生成扫描目标流
///
/// # 参数
/// - `subnets`: 子网列表
/// - `samples`: 每个子网的采样数
///
/// # 返回值
/// (子网, IP) 元组流
fn generate_scan_targets_stream(
    mut subnets: Vec<IpNet>,
    samples: usize,
) -> impl Stream<Item = (IpNet, IpAddr)> {
    stream! {
        let mut rng = StdRng::from_entropy();
        // 每一轮采样都打乱子网顺序，确保采样在时间分布和空间分布上更加均匀
        for _ in 0..samples {
            subnets.shuffle(&mut rng);
            for subnet in &subnets {
                let ip = generate_random_ip_in_subnet(subnet, &mut rng);
                yield (*subnet, ip);
            }
        }
    }
}

/// 创建进度条
///
/// # 参数
/// - `total`: 总任务数
///
/// # 返回值
/// 进度条实例
fn create_progress_bar(total: u64) -> ProgressBar {
    let pb = ProgressBar::new(total);
    let tpl = "{spinner:.green} [{elapsed_precise}] \
        [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}";
    pb.set_style(
        ProgressStyle::default_bar()
            .template(tpl)
            .unwrap()
            .progress_chars("#>-"),
    );
    pb.set_message("Scanning IPs...");
    pb
}

/// 批量写入结果
///
/// # 参数
/// - `batch`: 待写入的批次数据
/// - `results`: 结果存储容器
fn flush_batch(
    batch: &mut Vec<(IpNet, IpQuality)>,
    results: &Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>>,
) {
    if batch.is_empty() {
        return;
    }
    let mut guard = results.lock();
    for (subnet, quality) in batch.drain(..) {
        guard.entry(subnet).or_default().push(quality);
    }
}

/// 执行流式扫描
///
/// # 参数
/// - `targets`: 扫描目标流
/// - `total_count`: 总扫描数
/// - `concurrency_limit`: 并发限制
/// - `trace_url`: 探测 URL
/// - `results`: 结果存储容器
///
/// # 返回值
/// (成功数, 总扫描数)
async fn execute_scan_stream<S>(
    targets: S,
    total_count: usize,
    concurrency_limit: usize,
    trace_url: &str,
    results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>>,
    ip_manager: &IpManager,
) -> (usize, usize)
where
    S: Stream<Item = (IpNet, IpAddr)> + Unpin,
{
    if total_count == 0 {
        return (0, 0);
    }

    let pb = create_progress_bar(total_count as u64);
    let url = match reqwest::Url::parse(trace_url) {
        Ok(u) => u,
        Err(_) => return (0, 0),
    };
    let host = url.host_str().unwrap_or("").to_string();
    let port = url.port_or_known_default().unwrap_or(80);
    let trace_url_arc = Arc::new(trace_url.to_string());
    let host_arc = Arc::new(host);

    let mut stream = targets
        .map(|(subnet, ip)| {
            let url_ref = trace_url_arc.clone();
            let host_ref = host_arc.clone();
            let manager = ip_manager.clone();
            async move {
                let _permit = manager.acquire_fd_permit().await.ok();
                let quality = test_ip(ip, &url_ref, &host_ref, port).await;
                (subnet, quality)
            }
        })
        .buffer_unordered(concurrency_limit);

    let mut success_count = 0;
    let mut total_scanned = 0;
    let mut batch = Vec::with_capacity(100);

    while let Some((subnet, quality_opt)) = stream.next().await {
        total_scanned += 1;
        if let Some(quality) = quality_opt {
            success_count += 1;
            pb.set_message(format!("Success: {}", success_count));
            batch.push((subnet, quality));
            if batch.len() >= 100 {
                flush_batch(&mut batch, &results);
            }
        }
        pb.inc(1);
    }

    flush_batch(&mut batch, &results);
    pb.finish_with_message(format!("Scan complete. Found {} valid IPs.", success_count));
    (success_count, total_scanned)
}

/// 辅助迭代器，用于避开不必要的分配
enum SubnetIter {
    Single(std::iter::Once<IpNet>),
    Multiple(ipnet::IpSubnets),
}

impl Iterator for SubnetIter {
    type Item = IpNet;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Single(iter) => iter.next(),
            Self::Multiple(iter) => iter.next(),
        }
    }
}

/// 将 CIDR 列表拆分为指定掩码的子网
fn split_cidrs_to_subnets(cidrs: Vec<IpNet>, mask: u8) -> Vec<IpNet> {
    // 先聚合 CIDR，减少重叠和冗余
    let aggregated = IpNet::aggregate(&cidrs);

    aggregated
        .into_par_iter()
        .flat_map_iter(|cidr| {
            // 如果当前掩码已经大于或等于目标掩码，无需拆分
            if cidr.prefix_len() >= mask {
                return SubnetIter::Single(std::iter::once(cidr));
            }

            // 否则尝试拆分，如果失败则返回原 CIDR
            match cidr.subnets(mask) {
                Ok(subnets) => SubnetIter::Multiple(subnets),
                Err(_) => SubnetIter::Single(std::iter::once(cidr)),
            }
        })
        .collect()
}

/// 从 URL 获取 ASN CIDR 列表
///
/// # 参数
/// - `url`: ASN 数据 URL
///
/// # 返回值
/// CIDR 列表或错误
async fn fetch_asn_cidrs(url: &str) -> Result<Vec<IpNet>> {
    let client = Client::builder().timeout(Duration::from_secs(15)).build()?;

    let mut last_error = None;

    for attempt in 1..=ASN_MAX_RETRIES {
        let result = try_fetch_once(&client, url).await;
        if let Ok(cidrs) = result {
            return Ok(cidrs);
        }

        let e = result.unwrap_err();
        warn!("Failed to fetch ASN (try {}): {}", attempt, e);
        last_error = Some(e);

        if attempt < ASN_MAX_RETRIES {
            tokio::time::sleep(Duration::from_secs(ASN_RETRY_DELAY_SECS)).await;
        }
    }

    let err = last_error.unwrap_or_else(|| anyhow::anyhow!("Unknown error fetching ASN CIDRs"));
    Err(err)
}

/// 尝试单次获取 ASN CIDR 列表
async fn try_fetch_once(client: &Client, url: &str) -> Result<Vec<IpNet>> {
    let resp = client.get(url).send().await?;
    let text = resp.text().await?;
    Ok(parse_cidr_list(&text))
}

/// 解析 CIDR 列表文本
///
/// # 参数
/// - `text`: 包含 CIDR 的文本
///
/// # 返回值
/// 解析后的 CIDR 列表
fn parse_cidr_list(text: &str) -> Vec<IpNet> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            line.parse::<IpNet>().ok()
        })
        .collect()
}

/// 构建探测客户端
///
/// # 参数
/// - `ip`: 目标 IP
/// - `host`: 目标主机名
/// - `port`: 目标端口
///
/// # 返回值
/// 构建的客户端或 None
fn build_probe_client(ip: IpAddr, host: &str, port: u16) -> Option<Client> {
    Client::builder()
        .resolve(host, std::net::SocketAddr::new(ip, port))
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(1))
        .build()
        .ok()
}

/// 执行单次探测
///
/// # 参数
/// - `client`: HTTP 客户端
/// - `trace_url`: 探测 URL
/// - `probe_idx`: 探测索引
///
/// # 返回值
/// (延迟, Colo) 元组或 None
async fn execute_single_probe(
    client: &Client,
    trace_url: &str,
    probe_idx: usize,
) -> Option<(u128, String)> {
    let start = Instant::now();
    let response = match client.get(trace_url).send().await {
        Ok(resp) => resp,
        Err(e) => {
            debug!("Probe {} failed: {}", probe_idx + 1, e);
            return None;
        }
    };

    let latency = start.elapsed().as_millis();
    if !response.status().is_success() {
        return None;
    }

    let body = response.text().await.ok()?;
    let colo = parse_colo(&body)?;
    Some((latency, colo))
}

/// 执行探测循环
///
/// # 参数
/// - `client`: HTTP 客户端
/// - `trace_url`: 探测 URL
///
/// # 返回值
/// (延迟列表, 成功次数, 最后 Colo)
async fn run_probes(client: &Client, trace_url: &str) -> (Vec<u128>, usize, String) {
    let mut latencies = Vec::with_capacity(PROBE_COUNT);
    let mut success_count = 0;
    let mut last_colo = String::new();

    for probe_idx in 0..PROBE_COUNT {
        if let Some((latency, colo)) = execute_single_probe(client, trace_url, probe_idx).await {
            latencies.push(latency);
            success_count += 1;
            last_colo = colo;
        }

        if probe_idx < PROBE_COUNT - 1 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    (latencies, success_count, last_colo)
}

/// 测试单个 IP 的质量
///
/// # 参数
/// - `ip`: 目标 IP
/// - `trace_url`: 探测 URL
/// - `host`: 目标主机名
/// - `port`: 目标端口
///
/// # 返回值
/// IP 质量信息或 None
async fn test_ip(ip: IpAddr, trace_url: &str, host: &str, port: u16) -> Option<IpQuality> {
    let client = build_probe_client(ip, host, port)?;
    let (latencies, success_count, last_colo) = run_probes(&client, trace_url).await;

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

/// 计算平均值
///
/// # 参数
/// - `values`: 数值列表
///
/// # 返回值
/// 平均值
fn calculate_average(values: &[u128]) -> u128 {
    if values.is_empty() {
        return 0;
    }
    let sum = values.iter().fold(0u128, |acc, &v| acc.saturating_add(v));
    sum.checked_div(values.len() as u128).unwrap_or(0)
}

/// 计算抖动值
///
/// # 参数
/// - `latencies`: 延迟列表
///
/// # 返回值
/// 抖动值
fn calculate_jitter(latencies: &[u128]) -> u128 {
    if latencies.len() < 2 {
        return 0;
    }

    let sum_diff: i128 = latencies
        .windows(2)
        .map(|w| (w[0] as i128 - w[1] as i128).abs())
        .sum();

    let count = (latencies.len() - 1) as i128;
    (sum_diff.checked_div(count).unwrap_or(0)) as u128
}

/// 解析响应体中的 Colo 信息
///
/// # 参数
/// - `body`: 响应体文本
///
/// # 返回值
/// Colo 代码或 None
fn parse_colo(body: &str) -> Option<String> {
    body.lines()
        .find_map(|line| line.strip_prefix("colo=").map(|s| s.to_string()))
}

/// 执行一轮后台扫描并持久化结果
///
/// # 参数
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
///
/// # 返回值
/// 扫描统计信息
async fn run_and_persist(config: &Arc<AppConfig>, ip_manager: &IpManager) -> ScanStats {
    let concurrency = config.background_scan.concurrency;
    let stats = run_scan_once(config.clone(), ip_manager.clone(), concurrency).await;

    // 使用 spawn_blocking 避免阻塞 async 线程
    let manager = ip_manager.clone();
    let path = config.ip_store_file.clone();

    let save_result = tokio::task::spawn_blocking(move || manager.save_to_file(&path)).await;

    match save_result {
        Ok(Ok(_)) => info!("Background scan saved to {}", config.ip_store_file),
        Ok(Err(e)) => error!("Failed to save background scan: {}", e),
        Err(e) => error!("Failed to join save task: {}", e),
    }

    stats
}

/// 后台定时扫描主循环
///
/// # 参数
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
/// - `cancel_token`: 取消令牌
/// - `scan_on_start`: 是否启动时立即扫描
pub async fn run_background_scan(
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    cancel_token: CancellationToken,
    scan_on_start: bool,
) {
    let interval = Duration::from_secs(config.background_scan.interval_secs);

    info!(
        "Background scan enabled \
         (interval={}s, concurrency={}, scan_on_start={})",
        config.background_scan.interval_secs, config.background_scan.concurrency, scan_on_start,
    );

    if scan_on_start {
        info!("Running initial background scan...");
        let stats = run_and_persist(&config, &ip_manager).await;
        info!(
            "Initial scan done: {} scanned, {} ok",
            stats.total_scanned, stats.success_count,
        );
    }

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                info!("Background scan received shutdown");
                break;
            }
            _ = tokio::time::sleep(interval) => {
                info!("Starting scheduled background scan...");
                let stats = run_and_persist(
                    &config, &ip_manager,
                ).await;
                info!(
                    "Scheduled scan done: {} scanned, {} ok",
                    stats.total_scanned,
                    stats.success_count,
                );
            }
        }
    }

    info!("Background scan stopped");
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
    fn test_split_cidrs_to_subnets() {
        let cidrs = vec![
            "104.16.0.0/24".parse::<IpNet>().unwrap(),
            "104.16.1.0/24".parse::<IpNet>().unwrap(),
            "104.16.0.0/23".parse::<IpNet>().unwrap(), // Overlaps with the two above
        ];
        let subnets = split_cidrs_to_subnets(cidrs, 24);
        // Aggregation should merge them into 104.16.0.0/23, then split into two /24s
        assert_eq!(subnets.len(), 2);
        assert!(subnets.contains(&"104.16.0.0/24".parse::<IpNet>().unwrap()));
        assert!(subnets.contains(&"104.16.1.0/24".parse::<IpNet>().unwrap()));
    }

    #[test]
    fn test_split_cidrs_to_subnets_smaller_mask() {
        let cidrs = vec!["104.16.0.0/24".parse::<IpNet>().unwrap()];
        let subnets = split_cidrs_to_subnets(cidrs, 20);
        // Should not split, return the original
        assert_eq!(subnets.len(), 1);
        assert_eq!(subnets[0], "104.16.0.0/24".parse::<IpNet>().unwrap());
    }
}
