use crate::state::IpManager;
use std::collections::HashMap;

pub fn print_colo_ranking(ip_manager: &IpManager) {
    let subnets = ip_manager.get_all_subnets();
    if subnets.is_empty() {
        println!("No subnet data available. Please run with --scan first.");
        return;
    }

    struct ColoStats {
        total_score: f32,
        total_latency: u128,
        count: usize,
    }

    let mut stats_map: HashMap<String, ColoStats> = HashMap::new();

    for subnet in &subnets {
        let entry = stats_map.entry(subnet.colo.clone()).or_insert(ColoStats {
            total_score: 0.0,
            total_latency: 0,
            count: 0,
        });
        entry.total_score += subnet.score;
        entry.total_latency += subnet.avg_latency;
        entry.count += 1;
    }

    let mut ranking: Vec<(String, f32, u128, usize)> = stats_map
        .into_iter()
        .map(|(colo, stats)| {
            (
                colo,
                stats.total_score / stats.count as f32,
                if stats.count > 0 { stats.total_latency / stats.count as u128 } else { 0 },
                stats.count,
            )
        })
        .collect();

    // Sort by average score descending
    ranking.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    println!("{:<10} | {:<10} | {:<10} | {:<10}", "Colo", "Avg Score", "Avg Latency", "Subnet Count");
    println!("{:-<10}-|-{:-<10}-|-{:-<10}-|-{:-<10}", "", "", "", "");

    for (colo, avg_score, avg_latency, count) in ranking {
        println!("{:<10} | {:<10.2} | {:<10} | {:<10}", colo, avg_score, avg_latency, count);
    }
}