mod asn;
mod executor;
mod probe;

use crate::config::{AppConfig, ScanStrategyType};
use crate::model::{IpQuality, SubnetQuality};
use crate::state::IpManager;
use asn::fetch_asn_cidrs;
use executor::{
    aggregate_and_persist, execute_scan_stream, generate_scan_targets_stream,
    split_cidrs_to_subnets, FOCUSED_SCAN_SUBNET_MASK,
};
use ipnet::IpNet;
use parking_lot::Mutex;
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

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
        &config.scoring,
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
        &config.scoring,
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
        &config.scoring,
    )
    .await;

    info!(
        "[Phase 3/3] Completed: {}/{} IPs successful.",
        success, total
    );

    (success, total, results)
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
