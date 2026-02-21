use super::probe::test_ip;
use crate::config::{AppConfig, ScoringConfig};
use crate::model::{IpQuality, SubnetQuality};
use crate::state::IpManager;
use crate::utils::generate_random_ip_in_subnet;
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
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use tracing::info;

pub(super) const FOCUSED_SCAN_SUBNET_MASK: u8 = 24;

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
pub(super) fn split_cidrs_to_subnets(cidrs: Vec<IpNet>, mask: u8) -> Vec<IpNet> {
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

/// 生成扫描目标流
///
/// # 参数
/// - `subnets`: 子网列表
/// - `samples`: 每个子网的采样数
///
/// # 返回值
/// (子网, IP) 元组流
pub(super) fn generate_scan_targets_stream(
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
pub(super) async fn execute_scan_stream<S>(
    targets: S,
    total_count: usize,
    concurrency_limit: usize,
    trace_url: &str,
    results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>>,
    ip_manager: &IpManager,
    scoring: &ScoringConfig,
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
    let scoring = scoring.clone();

    let mut stream = targets
        .map(|(subnet, ip)| {
            let url_ref = trace_url_arc.clone();
            let host_ref = host_arc.clone();
            let manager = ip_manager.clone();
            let sc = scoring.clone();
            async move {
                let _permit = manager.acquire_fd_permit().await.ok();
                let quality = test_ip(ip, &url_ref, &host_ref, port, &sc).await;
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

/// 第四阶段：聚合与持久化
///
/// # 参数
/// - `results`: 扫描结果
/// - `ip_manager`: IP 管理器
/// - `top_k_percent`: 最佳子网筛选比例
///
/// # 返回值
/// 更新的子网数量
pub(super) fn aggregate_and_persist(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_cidrs_to_subnets() {
        let cidrs = vec![
            "104.16.0.0/24".parse::<IpNet>().unwrap(),
            "104.16.1.0/24".parse::<IpNet>().unwrap(),
            "104.16.0.0/23".parse::<IpNet>().unwrap(), // Overlaps
        ];
        let subnets = split_cidrs_to_subnets(cidrs, 24);
        // Aggregation should merge them into 104.16.0.0/23,
        // then split into two /24s
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
