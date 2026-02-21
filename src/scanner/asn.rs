use anyhow::Result;
use ipnet::IpNet;
use reqwest::Client;
use std::time::Duration;
use tracing::warn;

const ASN_MAX_RETRIES: usize = 3;
const ASN_RETRY_DELAY_SECS: u64 = 2;

/// 从 URL 获取 ASN CIDR 列表
///
/// # 参数
/// - `url`: ASN 数据 URL
///
/// # 返回值
/// CIDR 列表或错误
pub(super) async fn fetch_asn_cidrs(url: &str) -> Result<Vec<IpNet>> {
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
pub(super) fn parse_cidr_list(text: &str) -> Vec<IpNet> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cidr_list() {
        let text = "104.16.0.0/12\n# comment\n172.64.0.0/13\n\n";
        let cidrs = parse_cidr_list(text);
        assert_eq!(cidrs.len(), 2);
    }
}
