use crate::config::ScoringConfig;
use crate::model::IpQuality;
use reqwest::Client;
use std::net::IpAddr;
use std::time::{Duration, Instant};
use tracing::debug;

const TIMEOUT_SECS: u64 = 2;
const PROBE_COUNT: usize = 5;

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
pub(super) async fn test_ip(
    ip: IpAddr,
    trace_url: &str,
    host: &str,
    port: u16,
    scoring: &ScoringConfig,
) -> Option<IpQuality> {
    let client = build_probe_client(ip, host, port)?;
    let (latencies, success_count, last_colo) = run_probes(&client, trace_url).await;

    if success_count == 0 {
        return None;
    }

    let loss_rate = 1.0 - (success_count as f32 / PROBE_COUNT as f32);
    let avg_latency = calculate_average(&latencies);
    let jitter = calculate_jitter(&latencies);

    debug!(
        "Tested IP: {}, Colo: {}, Latency: {}ms, \
         Jitter: {}ms, Loss: {:.0}%",
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
        scoring,
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
    fn test_calculate_jitter() {
        let latencies = vec![100, 110, 105, 115];
        let jitter = calculate_jitter(&latencies);
        assert_eq!(jitter, 8);
    }
}
