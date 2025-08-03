use anyhow::Result;
use rand::prelude::*;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::RwLock;
use tracing::{debug, trace};

use crate::pools::PoolManager;
use crate::selector::ActiveRemotes;

/// 负载均衡算法枚举
#[derive(Debug, Clone, PartialEq)]
pub enum LoadBalanceAlgorithm {
    /// 最少连接数算法：选择当前活跃连接数最少的IP
    LeastConnections,
    /// 轮询算法：依次轮询选择IP
    RoundRobin,
    /// 随机算法：随机选择IP
    Random,
}

impl LoadBalanceAlgorithm {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "least_connections" => LoadBalanceAlgorithm::LeastConnections,
            "round_robin" => LoadBalanceAlgorithm::RoundRobin,
            "random" => LoadBalanceAlgorithm::Random,
            _ => {
                tracing::warn!("未知的负载均衡算法: {}, 使用默认的最少连接数算法", s);
                LoadBalanceAlgorithm::LeastConnections
            }
        }
    }
}

/// 负载均衡器状态管理
#[derive(Debug)]
pub struct LoadBalancerState {
    /// 轮询算法的当前索引
    round_robin_index: Arc<AtomicUsize>,
    /// 每个IP的连接计数器（用于最少连接数算法）
    connection_counts: Arc<RwLock<HashMap<IpAddr, Arc<AtomicUsize>>>>,
}

impl LoadBalancerState {
    pub fn new() -> Self {
        Self {
            round_robin_index: Arc::new(AtomicUsize::new(0)),
            connection_counts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 获取或创建IP的连接计数器
    pub async fn get_connection_counter(&self, ip: IpAddr) -> Arc<AtomicUsize> {
        let mut counts = self.connection_counts.write().await;
        counts
            .entry(ip)
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone()
    }

    /// 增加连接计数
    pub async fn increment_connection(&self, ip: IpAddr) {
        let counter = self.get_connection_counter(ip).await;
        counter.fetch_add(1, Ordering::Relaxed);
        trace!(
            "IP {} 连接数增加，当前: {}",
            ip,
            counter.load(Ordering::Relaxed)
        );
    }

    /// 减少连接计数
    pub async fn decrement_connection(&self, ip: IpAddr) {
        let counter = self.get_connection_counter(ip).await;
        let current = counter.load(Ordering::Relaxed);
        if current > 0 {
            counter.fetch_sub(1, Ordering::Relaxed);
            trace!(
                "IP {} 连接数减少，当前: {}",
                ip,
                counter.load(Ordering::Relaxed)
            );
        }
    }

    /// 获取IP的当前连接数
    pub async fn get_connection_count(&self, ip: IpAddr) -> usize {
        let counts = self.connection_counts.read().await;
        counts
            .get(&ip)
            .map(|counter| counter.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// 清理不活跃的IP计数器
    pub async fn cleanup_inactive_ips(&self, active_ips: &[IpAddr]) {
        let mut counts = self.connection_counts.write().await;
        let active_set: std::collections::HashSet<_> = active_ips.iter().collect();

        counts.retain(|ip, _| active_set.contains(ip));
        debug!("清理负载均衡器状态，保留 {} 个活跃IP", counts.len());
    }
}

/// 负载均衡器
#[derive(Debug)]
pub struct LoadBalancer {
    algorithm: LoadBalanceAlgorithm,
    state: LoadBalancerState,
}

impl LoadBalancer {
    pub fn new(algorithm: LoadBalanceAlgorithm) -> Self {
        Self {
            algorithm,
            state: LoadBalancerState::new(),
        }
    }

    /// 从活跃IP列表中选择一个目标IP
    pub async fn select_target_ip(
        &self,
        active_remotes: &ActiveRemotes,
        _pool_manager: Option<&PoolManager>, // 可用于获取更精确的连接统计信息
    ) -> Result<IpAddr> {
        let ips = active_remotes.read().await;

        if ips.is_empty() {
            return Err(anyhow::anyhow!("没有可用的活跃IP"));
        }

        let selected_ip = match self.algorithm {
            LoadBalanceAlgorithm::LeastConnections => self.select_least_connections(&ips).await,
            LoadBalanceAlgorithm::RoundRobin => self.select_round_robin(&ips).await,
            LoadBalanceAlgorithm::Random => self.select_random(&ips).await,
        };

        debug!(
            "使用 {:?} 算法选择目标IP: {}, 候选数量: {}",
            self.algorithm,
            selected_ip,
            ips.len()
        );

        Ok(selected_ip)
    }

    /// 最少连接数算法
    async fn select_least_connections(&self, ips: &[IpAddr]) -> IpAddr {
        let mut min_connections = usize::MAX;
        let mut selected_ip = ips[0];

        // 找到连接数最少的IP
        for &ip in ips {
            let count = self.state.get_connection_count(ip).await;
            if count < min_connections {
                min_connections = count;
                selected_ip = ip;
            }
        }

        trace!(
            "最少连接数算法选择 IP: {}, 连接数: {}",
            selected_ip, min_connections
        );

        selected_ip
    }

    /// 轮询算法
    async fn select_round_robin(&self, ips: &[IpAddr]) -> IpAddr {
        let index = self.state.round_robin_index.fetch_add(1, Ordering::Relaxed);
        let selected_index = index % ips.len();
        let selected_ip = ips[selected_index];

        trace!(
            "轮询算法选择 IP: {}, 索引: {}/{}",
            selected_ip,
            selected_index,
            ips.len()
        );

        selected_ip
    }

    /// 随机算法
    async fn select_random(&self, ips: &[IpAddr]) -> IpAddr {
        let mut rng = rand::rng();
        let selected_ip = *ips.choose(&mut rng).unwrap();

        trace!("随机算法选择 IP: {}", selected_ip);

        selected_ip
    }

    /// 记录连接开始（增加计数）
    pub async fn on_connection_start(&self, ip: IpAddr) {
        if matches!(self.algorithm, LoadBalanceAlgorithm::LeastConnections) {
            self.state.increment_connection(ip).await;
        }
    }

    /// 记录连接结束（减少计数）
    pub async fn on_connection_end(&self, ip: IpAddr) {
        if matches!(self.algorithm, LoadBalanceAlgorithm::LeastConnections) {
            self.state.decrement_connection(ip).await;
        }
    }

    /// 清理不活跃的IP
    pub async fn cleanup_inactive_ips(&self, active_ips: &[IpAddr]) {
        self.state.cleanup_inactive_ips(active_ips).await;
    }

    /// 获取算法名称
    pub fn algorithm_name(&self) -> &str {
        match self.algorithm {
            LoadBalanceAlgorithm::LeastConnections => "least_connections",
            LoadBalanceAlgorithm::RoundRobin => "round_robin",
            LoadBalanceAlgorithm::Random => "random",
        }
    }

    /// 获取所有IP的连接统计信息
    pub async fn get_connection_stats(&self) -> HashMap<IpAddr, usize> {
        let mut stats = HashMap::new();
        let counts = self.state.connection_counts.read().await;

        for (&ip, counter) in counts.iter() {
            stats.insert(ip, counter.load(Ordering::Relaxed));
        }

        stats
    }
}

/// 负载均衡器管理任务
pub async fn load_balancer_manager_task(
    load_balancer: Arc<LoadBalancer>,
    active_remotes: ActiveRemotes,
) {
    let mut cleanup_interval = tokio::time::interval(std::time::Duration::from_secs(30));

    loop {
        cleanup_interval.tick().await;

        // 获取当前活跃IP列表
        let active_ips = {
            let ips = active_remotes.read().await;
            ips.clone()
        };

        // 清理不活跃的IP统计信息
        load_balancer.cleanup_inactive_ips(&active_ips).await;

        // 如果是最少连接数算法，定期输出统计信息
        if matches!(
            load_balancer.algorithm,
            LoadBalanceAlgorithm::LeastConnections
        ) {
            let stats = load_balancer.get_connection_stats().await;
            if !stats.is_empty() {
                debug!("负载均衡器连接统计: {:?}", stats);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[tokio::test]
    async fn test_least_connections_selection() {
        let lb = LoadBalancer::new(LoadBalanceAlgorithm::LeastConnections);
        let ips = vec![
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 3)),
        ];

        // 初始状态下应该选择第一个IP（都是0连接）
        let selected = lb.select_least_connections(&ips).await;
        assert_eq!(selected, ips[0]);

        // 增加第一个IP的连接数
        lb.state.increment_connection(ips[0]).await;

        // 现在应该选择第二个IP
        let selected = lb.select_least_connections(&ips).await;
        assert_eq!(selected, ips[1]);
    }

    #[tokio::test]
    async fn test_round_robin_selection() {
        let lb = LoadBalancer::new(LoadBalanceAlgorithm::RoundRobin);
        let ips = vec![
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 3)),
        ];

        // 轮询应该依次选择每个IP
        for i in 0..6 {
            let selected = lb.select_round_robin(&ips).await;
            assert_eq!(selected, ips[i % ips.len()]);
        }
    }

    #[tokio::test]
    async fn test_random_selection() {
        let lb = LoadBalancer::new(LoadBalanceAlgorithm::Random);
        let ips = vec![
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 3)),
        ];

        // 随机选择应该从IP列表中选择一个
        for _ in 0..10 {
            let selected = lb.select_random(&ips).await;
            assert!(ips.contains(&selected));
        }
    }
}
