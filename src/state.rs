use crate::config::ScoringConfig;
use crate::model::SubnetQuality;
use crate::utils::generate_random_ip_in_subnet;
use anyhow::Result;
use arc_swap::ArcSwap;
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use ipnet::IpNet;
use rand::prelude::*;
use rayon::prelude::*;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, warn};

/// IP 管理器 - 管理子网质量数据和 IP 选择
#[derive(Clone)]
pub struct IpManager {
    /// 存储子网质量数据，以子网为键
    subnets: Arc<DashMap<IpNet, SubnetQuality>>,
    /// 最佳子网缓存（top K%）
    best_subnets_cache: Arc<ArcSwap<Vec<IpNet>>>,
    /// 按 colo 分组的最佳子网缓存
    best_by_colo: Arc<ArcSwap<HashMap<String, Vec<IpNet>>>>,
    /// 全量子网按评分降序排列的缓存（Web 接口用）
    sorted_subnets_cache: Arc<ArcSwap<Vec<SubnetQuality>>>,
    /// 全局资源限制（FD 信号量）
    fd_semaphore: Arc<Semaphore>,
}

impl Default for IpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl IpManager {
    /// 创建新的 IP 管理器
    pub fn new() -> Self {
        Self {
            subnets: Arc::new(DashMap::new()),
            best_subnets_cache: Arc::new(ArcSwap::from_pointee(Vec::new())),
            best_by_colo: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            sorted_subnets_cache: Arc::new(ArcSwap::from_pointee(Vec::new())),
            fd_semaphore: Arc::new(Semaphore::new(1024)),
        }
    }

    /// 更新子网质量数据
    pub fn update_subnet(&self, quality: SubnetQuality) {
        debug!(
            "Updating subnet {} with score {:.2}",
            quality.subnet, quality.score
        );
        self.subnets.insert(quality.subnet, quality);
    }

    /// 使用 EMA (指数移动平均) 更新子网质量指标
    pub fn update_subnet_metrics_ema(
        &self,
        subnet: &IpNet,
        latency_ms: u64,
        is_success: bool,
        alpha: f64,
        cfg: &ScoringConfig,
    ) {
        if let Some(mut entry) = self.subnets.get_mut(subnet) {
            let quality = entry.value_mut();

            let new_latency = latency_ms as f64;
            let mut new_loss_rate = 1.0;
            if is_success {
                new_loss_rate = 0.0;
            }

            quality.avg_latency =
                (alpha * new_latency + (1.0 - alpha) * (quality.avg_latency as f64)) as u64;
            quality.avg_loss_rate =
                (alpha * new_loss_rate + (1.0 - alpha) * (quality.avg_loss_rate as f64)) as f32;

            let latency_penalty =
                (quality.avg_latency as f32) / 10.0 * cfg.latency_penalty_per_10ms;
            let jitter_penalty = (quality.avg_jitter as f32) / 5.0 * cfg.jitter_penalty_per_5ms;
            let loss_penalty = quality.avg_loss_rate * 100.0 * cfg.loss_penalty_per_percent;

            quality.score =
                (cfg.base_score - latency_penalty - jitter_penalty - loss_penalty).max(0.0);
            quality.last_updated = Utc::now();
        }
    }

    pub fn cleanup_subnets(
        &self,
        now: DateTime<Utc>,
        ttl_secs: u64,
        max_subnets: usize,
    ) -> (usize, usize) {
        let removed_expired = self.remove_expired(now, ttl_secs);
        let removed_evicted = self.evict_if_needed(max_subnets);
        (removed_expired, removed_evicted)
    }

    fn remove_expired(&self, now: DateTime<Utc>, ttl_secs: u64) -> usize {
        if ttl_secs == 0 {
            return 0;
        }

        let ttl = Duration::seconds(ttl_secs as i64);
        let cutoff = now - ttl;

        let mut removed = 0usize;
        let keys: Vec<IpNet> = self
            .subnets
            .iter()
            .filter_map(|e| {
                let updated = e.value().last_updated;
                if updated < cutoff {
                    return Some(*e.key());
                }
                None
            })
            .collect();

        for key in keys {
            if self.subnets.remove(&key).is_some() {
                removed = removed.saturating_add(1);
            }
        }

        removed
    }

    fn evict_if_needed(&self, max_subnets: usize) -> usize {
        let current = self.subnets.len();
        if current <= max_subnets {
            return 0;
        }

        let need_remove = current - max_subnets;
        let mut entries: Vec<(IpNet, f32, DateTime<Utc>)> = self
            .subnets
            .iter()
            .map(|e| (*e.key(), e.value().score, e.value().last_updated))
            .collect();

        entries.par_sort_by(|a, b| {
            let score_cmp = a.1.total_cmp(&b.1);
            if score_cmp != std::cmp::Ordering::Equal {
                return score_cmp;
            }
            a.2.cmp(&b.2)
        });

        let mut removed = 0usize;
        for (subnet, _, _) in entries.into_iter().take(need_remove) {
            if self.subnets.remove(&subnet).is_some() {
                removed = removed.saturating_add(1);
            }
        }
        removed
    }

    /// 重新计算最佳子网缓存
    pub fn recalculate_best_subnets(&self, top_k_percent: f64) {
        let mut all_subnets: Vec<SubnetQuality> =
            self.subnets.iter().map(|e| e.value().clone()).collect();

        if all_subnets.is_empty() {
            self.best_subnets_cache.store(Arc::new(Vec::new()));
            self.best_by_colo.store(Arc::new(HashMap::new()));
            self.sorted_subnets_cache.store(Arc::new(Vec::new()));
            return;
        }

        all_subnets.par_sort_by(|a, b| b.score.total_cmp(&a.score));

        let total = all_subnets.len();
        let k = calculate_top_k(total, top_k_percent);

        let top_slice = &all_subnets[..k];
        let top_subnets: Vec<IpNet> = top_slice.iter().map(|q| q.subnet).collect();

        let mut colo_map: HashMap<String, Vec<IpNet>> = HashMap::new();
        for q in top_slice {
            colo_map.entry(q.colo.clone()).or_default().push(q.subnet);
        }

        debug!(
            "Recalculated best subnets: {} of {} (top {:.1}%)",
            k,
            total,
            top_k_percent * 100.0
        );

        self.best_subnets_cache.store(Arc::new(top_subnets));
        self.best_by_colo.store(Arc::new(colo_map));
        // 全量排序结果写入缓存，供 Web 接口零拷贝读取
        self.sorted_subnets_cache.store(Arc::new(all_subnets));
    }

    pub fn best_cache_len(&self) -> usize {
        self.best_subnets_cache.load().len()
    }

    /// 获取按评分降序排列的全量子网缓存（零拷贝）
    ///
    /// 缓存在 recalculate_best_subnets 中更新，
    /// Web 接口直接读取。
    ///
    /// # 返回值
    /// Arc 引用的排序子网列表
    pub fn get_sorted_subnets(&self) -> arc_swap::Guard<Arc<Vec<SubnetQuality>>> {
        self.sorted_subnets_cache.load()
    }

    /// 获取用于转发的目标 IP 列表
    ///
    /// 策略：
    /// 1. 从缓存的最佳子网中按 colo 过滤
    /// 2. 随机选择 n 个子网
    /// 3. 从每个选中的子网中随机生成 m 个 IP
    pub fn get_target_ips(
        &self,
        target_colos: &[String],
        n_subnets: usize,
        m_ips: usize,
    ) -> Vec<(IpAddr, IpNet)> {
        let candidates = self.get_colo_filtered_subnets(target_colos);

        if candidates.is_empty() {
            debug!("No candidates after colo filtering");
            return Vec::new();
        }

        let mut rng = rand::thread_rng();
        let selected = self.select_subnets(&candidates, n_subnets, &mut rng);

        self.generate_ips_from_subnets(&selected, m_ips, &mut rng)
    }

    /// 从缓存中获取 colo 过滤后的子网列表
    fn get_colo_filtered_subnets(&self, target_colos: &[String]) -> Vec<IpNet> {
        if target_colos.is_empty() {
            let best = self.best_subnets_cache.load();
            return (**best).clone();
        }

        let colo_map = self.best_by_colo.load();
        let mut result = Vec::new();
        for colo in target_colos {
            if let Some(subnets) = colo_map.get(colo) {
                result.extend_from_slice(subnets);
            }
        }
        result
    }

    /// 随机选择子网
    fn select_subnets(&self, candidates: &[IpNet], n: usize, rng: &mut impl Rng) -> Vec<IpNet> {
        if candidates.len() <= n {
            return candidates.to_vec();
        }
        candidates.choose_multiple(rng, n).cloned().collect()
    }

    /// 从子网生成 IP
    fn generate_ips_from_subnets(
        &self,
        subnets: &[IpNet],
        m_ips: usize,
        rng: &mut impl Rng,
    ) -> Vec<(IpAddr, IpNet)> {
        let mut target_ips = Vec::with_capacity(subnets.len() * m_ips);
        for subnet in subnets {
            for _ in 0..m_ips {
                target_ips.push((generate_random_ip_in_subnet(subnet, rng), *subnet));
            }
        }

        debug!("Generated {} target IPs", target_ips.len());
        target_ips
    }

    /// 获取所有子网质量数据
    pub fn get_all_subnets(&self) -> Vec<SubnetQuality> {
        self.subnets
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// 获取子网数量
    pub fn subnet_count(&self) -> usize {
        self.subnets.len()
    }

    /// 设置最大打开文件数（动态调整信号量）
    pub fn set_max_open_files(&self, max: usize) {
        let current = self.fd_semaphore.available_permits();
        if max > current {
            self.fd_semaphore.add_permits(max - current);
            debug!("Increased FD semaphore permits to {}", max);
            return;
        }
        if max < current {
            let diff = current - max;
            if let Ok(permit) = self.fd_semaphore.try_acquire_many(diff as u32) {
                permit.forget();
                debug!("Decreased FD semaphore permits to {}", max);
                return;
            }
            warn!("Failed to decrease FD semaphore permits immediately");
        }
    }

    /// 获取 FD 许可
    pub async fn acquire_fd_permit(&self) -> Result<OwnedSemaphorePermit> {
        self.fd_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to acquire FD permit: {}", e))
    }

    /// 保存到文件
    pub fn save_to_file(&self, path: &str) -> Result<()> {
        let subnets: Vec<SubnetQuality> = self.get_all_subnets();
        let file = std::fs::File::create(path)?;
        let writer = std::io::BufWriter::new(file);
        serde_json::to_writer_pretty(writer, &subnets)?;
        Ok(())
    }

    /// 从文件加载
    pub fn load_from_file(&self, path: &str, top_k_percent: f64) -> Result<()> {
        let file = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(file);
        let subnets: Vec<SubnetQuality> = serde_json::from_reader(reader)?;

        self.subnets.clear();
        for subnet in subnets {
            self.subnets.insert(subnet.subnet, subnet);
        }

        // 立即刷新缓存
        self.recalculate_best_subnets(top_k_percent);

        Ok(())
    }
}

/// 计算 top K 的数量
///
/// 根据总数和百分比计算需要保留的数量
fn calculate_top_k(total: usize, percent: f64) -> usize {
    let k = (total as f64 * percent).ceil() as usize;
    k.clamp(1, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_top_k() {
        assert_eq!(calculate_top_k(100, 0.1), 10);
        assert_eq!(calculate_top_k(100, 0.01), 1);
        assert_eq!(calculate_top_k(100, 1.0), 100);
        assert_eq!(calculate_top_k(5, 0.1), 1); // 至少 1
    }

    #[test]
    fn test_ip_manager_new() {
        let manager = IpManager::new();
        assert_eq!(manager.subnet_count(), 0);
    }

    #[tokio::test]
    async fn test_fd_semaphore() {
        let manager = IpManager::new();
        manager.set_max_open_files(2);

        let permit1 = manager.acquire_fd_permit().await;
        assert!(permit1.is_ok());

        let permit2 = manager.acquire_fd_permit().await;
        assert!(permit2.is_ok());

        // 第三次获取应该阻塞，我们用 timeout 测试
        let permit3 = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            manager.acquire_fd_permit(),
        )
        .await;
        assert!(permit3.is_err());

        drop(permit1);

        let permit3 = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            manager.acquire_fd_permit(),
        )
        .await;
        assert!(permit3.is_ok());
    }

    /// 辅助函数：创建指定参数的子网质量数据
    ///
    /// # 参数
    /// - `subnet_str`: 子网字符串（如 "1.2.3.0/24"）
    /// - `latency`: 平均延迟（毫秒）
    /// - `loss_rate`: 丢包率（0.0-1.0）
    /// - `score`: 初始评分
    ///
    /// # 返回值
    /// SubnetQuality 实例
    fn make_subnet_quality(
        subnet_str: &str,
        latency: u64,
        loss_rate: f32,
        score: f32,
    ) -> SubnetQuality {
        SubnetQuality {
            subnet: subnet_str.parse().unwrap(),
            score,
            avg_latency: latency,
            avg_jitter: 10,
            avg_loss_rate: loss_rate,
            sample_count: 5,
            colo: "LAX".to_string(),
            last_updated: Utc::now(),
        }
    }

    /// 测试成功连接后 EMA 更新
    ///
    /// 初始子网延迟 200ms、丢包 0.5，
    /// 成功连接 50ms 后延迟应下降、丢包应下降、
    /// 分数应提升。
    #[test]
    fn test_ema_update_success() {
        let manager = IpManager::new();
        let cfg = ScoringConfig::default();
        let subnet_str = "10.0.0.0/24";
        let subnet: IpNet = subnet_str.parse().unwrap();

        let quality = make_subnet_quality(subnet_str, 200, 0.01, 28.0);
        let old_latency = quality.avg_latency;
        let old_loss = quality.avg_loss_rate;
        let old_score = quality.score;
        manager.update_subnet(quality);

        // 成功连接，延迟 50ms
        manager.update_subnet_metrics_ema(&subnet, 50, true, 0.3, &cfg);

        let entry = manager.subnets.get(&subnet).unwrap();
        let q = entry.value();

        // 延迟应下降
        assert!(
            q.avg_latency < old_latency,
            "latency should decrease: {} < {}",
            q.avg_latency,
            old_latency,
        );
        // 丢包率应下降
        assert!(
            q.avg_loss_rate < old_loss,
            "loss should decrease: {} < {}",
            q.avg_loss_rate,
            old_loss,
        );
        // 分数应提升
        assert!(
            q.score > old_score,
            "score should increase: {} > {}",
            q.score,
            old_score,
        );
    }

    /// 测试失败连接后 EMA 更新
    ///
    /// 初始子网延迟 100ms、丢包 0.0、高分，
    /// 失败连接 2000ms 后延迟应上升、丢包应上升、
    /// 分数应下降。
    #[test]
    fn test_ema_update_failure() {
        let manager = IpManager::new();
        let cfg = ScoringConfig::default();
        let subnet_str = "10.1.0.0/24";
        let subnet: IpNet = subnet_str.parse().unwrap();

        let quality = make_subnet_quality(subnet_str, 100, 0.0, 90.0);
        let old_latency = quality.avg_latency;
        let old_loss = quality.avg_loss_rate;
        let old_score = quality.score;
        manager.update_subnet(quality);

        // 失败连接，延迟 2000ms
        manager.update_subnet_metrics_ema(&subnet, 2000, false, 0.3, &cfg);

        let entry = manager.subnets.get(&subnet).unwrap();
        let q = entry.value();

        // 延迟应上升
        assert!(
            q.avg_latency > old_latency,
            "latency should increase: {} > {}",
            q.avg_latency,
            old_latency,
        );
        // 丢包率应上升
        assert!(
            q.avg_loss_rate > old_loss,
            "loss should increase: {} > {}",
            q.avg_loss_rate,
            old_loss,
        );
        // 分数应下降
        assert!(
            q.score < old_score,
            "score should decrease: {} < {}",
            q.score,
            old_score,
        );
    }

    /// 测试对不存在的子网调用 EMA 更新不会 panic
    #[test]
    fn test_ema_update_unknown_subnet() {
        let manager = IpManager::new();
        let cfg = ScoringConfig::default();
        let subnet: IpNet = "192.168.0.0/24".parse().unwrap();

        // 子网不存在，调用不应 panic
        manager.update_subnet_metrics_ema(&subnet, 100, true, 0.3, &cfg);

        // 子网仍不存在
        assert_eq!(manager.subnet_count(), 0);
    }
}
