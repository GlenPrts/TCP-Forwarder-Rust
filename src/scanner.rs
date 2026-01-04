use crate::config::AppConfig;
use crate::model::{IpQuality, SubnetQuality};
use crate::state::IpManager;
use crate::utils::generate_random_ip_in_subnet;
use anyhow::Result;
use ipnet::IpNet;
use reqwest::Client;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, info, warn};

/// 请求超时时间（秒）
const TIMEOUT_SECS: u64 = 2;
/// 每个 IP 的探测次数
const PROBE_COUNT: usize = 5;
/// IPv4 子网掩码（用于分组）
const SUBNET_MASK: u8 = 24;
/// 每个子网采样的 IP 数量
const SAMPLES_PER_SUBNET: usize = 3;
/// ASN 请求最大重试次数
const ASN_MAX_RETRIES: usize = 3;
/// ASN 请求重试间隔（秒）
const ASN_RETRY_DELAY_SECS: u64 = 2;

/// 扫描结果统计
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

/// 执行一次完整的扫描
pub async fn run_scan_once(
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    semaphore: Arc<Semaphore>,
) -> ScanStats {
    let mut root_cidrs = config.cidr_list.clone();

    // 尝试从 ASN 服务获取最新的 CIDR 列表
    match fetch_asn_cidrs(&config.asn_url).await {
        Ok(new_cidrs) => {
            if !new_cidrs.is_empty() {
                info!("Updated CIDR list with {} subnets from ASN", new_cidrs.len());
                root_cidrs = new_cidrs;
            }
        }
        Err(e) => {
            warn!("Failed to fetch ASN CIDRs, using cached list: {}", e);
        }
    }

    info!(
        "Starting scan with {} root CIDRs, semaphore permits: {}",
        root_cidrs.len(),
        semaphore.available_permits()
    );

    // 1. 将大 CIDR 拆分为子网（如 /24）
    let target_subnets = split_cidrs_to_subnets(&root_cidrs, SUBNET_MASK);
    info!("Total target subnets to scan: {}", target_subnets.len());

    // 2. 准备扫描任务
    let subnet_results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>> =
        Arc::new(Mutex::new(HashMap::with_capacity(target_subnets.len())));

    let tasks = create_scan_tasks(
        &target_subnets,
        &config.trace_url,
        semaphore,
        subnet_results.clone(),
    );

    info!("Created {} scan tasks", tasks.len());

    // 3. 执行所有任务并收集结果
    let (success_count, total_scanned) = execute_scan_tasks(tasks).await;

    // 4. 聚合结果并更新 IpManager
    let results = subnet_results.lock().await;
    let mut updated_subnets = 0;

    for (subnet, samples) in results.iter() {
        if !samples.is_empty() {
            let quality = SubnetQuality::new(*subnet, samples);
            ip_manager.update_subnet(quality);
            updated_subnets += 1;
        }
    }

    // 5. 更新 top K% 缓存
    ip_manager.recalculate_best_subnets(config.selection_top_k_percent);

    let stats = ScanStats {
        total_scanned,
        success_count,
        updated_subnets,
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

/// 将 CIDR 列表拆分为指定掩码的子网
fn split_cidrs_to_subnets(cidrs: &[IpNet], mask: u8) -> Vec<IpNet> {
    let mut target_subnets = Vec::new();
    for cidr in cidrs {
        match cidr.subnets(mask) {
            Ok(subnets) => target_subnets.extend(subnets),
            Err(_) => target_subnets.push(*cidr), // 已经小于目标掩码，保持原样
        }
    }
    target_subnets
}

/// 创建扫描任务
fn create_scan_tasks(
    subnets: &[IpNet],
    trace_url: &str,
    semaphore: Arc<Semaphore>,
    results: Arc<Mutex<HashMap<IpNet, Vec<IpQuality>>>>,
) -> Vec<tokio::task::JoinHandle<bool>> {
    let mut tasks = Vec::with_capacity(subnets.len() * SAMPLES_PER_SUBNET);
    let mut rng = rand::thread_rng();
    let trace_url = Arc::new(trace_url.to_string());

    for subnet in subnets {
        for _ in 0..SAMPLES_PER_SUBNET {
            let ip = generate_random_ip_in_subnet(subnet, &mut rng);
            let subnet_clone = *subnet;
            let semaphore_clone = semaphore.clone();
            let results_clone = results.clone();
            let trace_url_clone = trace_url.clone();

            tasks.push(tokio::spawn(async move {
                // 获取信号量许可，如果失败则跳过
                let _permit = match semaphore_clone.acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => return false,
                };

                if let Some(quality) = test_ip(ip, &trace_url_clone).await {
                    let mut results = results_clone.lock().await;
                    results
                        .entry(subnet_clone)
                        .or_insert_with(Vec::new)
                        .push(quality);
                    return true;
                }
                false
            }));
        }
    }

    tasks
}

/// 执行扫描任务并返回统计信息
async fn execute_scan_tasks(tasks: Vec<tokio::task::JoinHandle<bool>>) -> (usize, usize) {
    let mut success_count = 0;
    let mut total_scanned = 0;

    for task in tasks {
        match task.await {
            Ok(success) => {
                total_scanned += 1;
                if success {
                    success_count += 1;
                }
            }
            Err(e) => {
                debug!("Scan task failed: {}", e);
            }
        }
    }

    (success_count, total_scanned)
}

/// 从 ASN 服务获取 CIDR 列表
async fn fetch_asn_cidrs(url: &str) -> Result<Vec<IpNet>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

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

/// 解析 CIDR 列表文本
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

/// 测试单个 IP 的质量
async fn test_ip(ip: IpAddr, trace_url: &str) -> Option<IpQuality> {
    // 从 trace_url 提取主机名用于 DNS 解析
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
        // 探测之间的小延迟
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

    Some(IpQuality::new(ip, avg_latency, jitter, loss_rate, last_colo))
}

/// 计算平均值
fn calculate_average(values: &[u128]) -> u128 {
    if values.is_empty() {
        0
    } else {
        values.iter().sum::<u128>() / values.len() as u128
    }
}

/// 计算抖动（相邻值差的平均值）
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

/// 从 trace 响应中解析 colo 信息
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
        // |100-110| + |110-105| + |105-115| = 10 + 5 + 10 = 25, avg = 25/3 = 8
        assert_eq!(jitter, 8);
    }

    #[test]
    fn test_calculate_average() {
        let values = vec![100, 200, 300];
        assert_eq!(calculate_average(&values), 200);
        assert_eq!(calculate_average(&[]), 0);
    }
}