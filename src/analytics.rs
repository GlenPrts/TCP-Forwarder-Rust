use crate::model::SubnetQuality;
use crate::state::IpManager;
use std::collections::HashMap;

/// 数据中心统计信息
#[derive(Debug, Default)]
struct ColoStats {
    total_score: f32,
    total_latency: u64,
    total_jitter: u64,
    total_loss_rate: f32,
    count: usize,
}

impl ColoStats {
    fn add(&mut self, subnet: &SubnetQuality) {
        self.total_score += subnet.score;
        // 使用 saturating_add 防止整数溢出
        self.total_latency = self.total_latency.saturating_add(subnet.avg_latency);
        self.total_jitter = self.total_jitter.saturating_add(subnet.avg_jitter);
        self.total_loss_rate += subnet.avg_loss_rate;
        self.count = self.count.saturating_add(1);
    }

    fn avg_score(&self) -> f32 {
        if self.count == 0 {
            0.0
        } else {
            self.total_score / self.count as f32
        }
    }

    fn avg_latency(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            self.total_latency / self.count as u64
        }
    }

    fn avg_jitter(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            self.total_jitter / self.count as u64
        }
    }

    fn avg_loss_rate(&self) -> f32 {
        if self.count == 0 {
            0.0
        } else {
            self.total_loss_rate / self.count as f32
        }
    }
}

/// 数据中心排名条目
#[derive(Debug)]
pub struct ColoRankingEntry {
    pub colo: String,
    pub avg_score: f32,
    pub avg_latency: u64,
    pub avg_jitter: u64,
    pub avg_loss_rate: f32,
    pub subnet_count: usize,
}

/// 获取数据中心排名
pub fn get_colo_ranking(ip_manager: &IpManager) -> Vec<ColoRankingEntry> {
    let subnets = ip_manager.get_all_subnets();

    let mut stats_map: HashMap<String, ColoStats> = HashMap::new();

    for subnet in &subnets {
        stats_map
            .entry(subnet.colo.clone())
            .or_default()
            .add(subnet);
    }

    let mut ranking: Vec<ColoRankingEntry> = stats_map
        .into_iter()
        .map(|(colo, stats)| ColoRankingEntry {
            colo,
            avg_score: stats.avg_score(),
            avg_latency: stats.avg_latency(),
            avg_jitter: stats.avg_jitter(),
            avg_loss_rate: stats.avg_loss_rate(),
            subnet_count: stats.count,
        })
        .collect();

    // 按平均评分降序排序
    ranking.sort_by(|a, b| b.avg_score.total_cmp(&a.avg_score));

    ranking
}

/// 打印数据中心排名
pub fn print_colo_ranking(ip_manager: &IpManager) {
    let subnets = ip_manager.get_all_subnets();
    if subnets.is_empty() {
        println!("No subnet data available. Please run with --scan first.");
        return;
    }

    let ranking = get_colo_ranking(ip_manager);

    // 打印表头
    println!(
        "{:<8} | {:>10} | {:>12} | {:>10} | {:>10} | {:>12}",
        "Colo", "Avg Score", "Avg Latency", "Avg Jitter", "Loss Rate", "Subnet Count"
    );
    println!("{}", "-".repeat(75));

    // 打印数据
    for entry in ranking {
        println!(
            "{:<8} | {:>10.2} | {:>10} ms | {:>8} ms | {:>9.2}% | {:>12}",
            entry.colo,
            entry.avg_score,
            entry.avg_latency,
            entry.avg_jitter,
            entry.avg_loss_rate * 100.0,
            entry.subnet_count
        );
    }

    println!();
    println!("Total subnets: {}", subnets.len());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SubnetQuality;
    use chrono::Utc;
    use ipnet::IpNet;

    fn create_test_subnet(colo: &str, score: f32, latency: u64) -> SubnetQuality {
        SubnetQuality {
            subnet: "1.2.3.0/24".parse::<IpNet>().unwrap(),
            score,
            avg_latency: latency,
            avg_jitter: 10,
            avg_loss_rate: 0.0,
            sample_count: 1,
            colo: colo.to_string(),
            last_updated: Utc::now(),
        }
    }

    #[test]
    fn test_colo_stats() {
        let mut stats = ColoStats::default();

        let subnet1 = create_test_subnet("LAX", 80.0, 100);
        let subnet2 = create_test_subnet("LAX", 90.0, 120);

        stats.add(&subnet1);
        stats.add(&subnet2);

        assert_eq!(stats.count, 2);
        assert_eq!(stats.avg_score(), 85.0);
        assert_eq!(stats.avg_latency(), 110);
    }
}
