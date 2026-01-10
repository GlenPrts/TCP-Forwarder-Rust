use crate::model::SubnetQuality;
use crate::utils::generate_random_ip_in_subnet;
use anyhow::Result;
use dashmap::DashMap;
use ipnet::IpNet;
use parking_lot::RwLock;
use rand::prelude::*;
use std::net::IpAddr;
use std::sync::Arc;
use tracing::debug;

/// IP 管理器 - 管理子网质量数据和 IP 选择
#[derive(Clone)]
pub struct IpManager {
    /// 存储子网质量数据，以子网为键
    subnets: Arc<DashMap<IpNet, SubnetQuality>>,
    /// 最佳子网缓存（top K%）
    best_subnets_cache: Arc<RwLock<Vec<IpNet>>>,
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
            best_subnets_cache: Arc::new(RwLock::new(Vec::new())),
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

    /// 重新计算最佳子网缓存
    pub fn recalculate_best_subnets(&self, top_k_percent: f64) {
        let mut all_subnets: Vec<SubnetQuality> =
            self.subnets.iter().map(|e| e.value().clone()).collect();

        if all_subnets.is_empty() {
            let mut cache = self.best_subnets_cache.write();
            cache.clear();
            return;
        }

        // 按评分降序排序
        all_subnets.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let total = all_subnets.len();
        let k = calculate_top_k(total, top_k_percent);

        let top_subnets: Vec<IpNet> = all_subnets.iter().take(k).map(|q| q.subnet).collect();

        debug!(
            "Recalculated best subnets: {} out of {} (top {:.1}%)",
            k,
            total,
            top_k_percent * 100.0
        );

        {
            let mut cache = self.best_subnets_cache.write();
            *cache = top_subnets;
        }
    }

    /// 获取用于转发的目标 IP 列表
    ///
    /// 策略：
    /// 1. 从缓存的最佳子网中按 colo 过滤（如果指定）
    /// 2. 随机选择 n 个子网
    /// 3. 从每个选中的子网中随机生成 m 个 IP
    pub fn get_target_ips(
        &self,
        target_colos: &[String],
        n_subnets: usize,
        m_ips: usize,
    ) -> Vec<IpAddr> {
        let best_subnets = self.best_subnets_cache.read();

        if best_subnets.is_empty() {
            debug!("Best subnets cache is empty");
            return Vec::new();
        }

        // 按 colo 过滤
        let candidates: Vec<IpNet> = if target_colos.is_empty() {
            best_subnets.clone()
        } else {
            best_subnets
                .iter()
                .filter(|&subnet| {
                    self.subnets
                        .get(subnet)
                        .map(|entry| target_colos.contains(&entry.value().colo))
                        .unwrap_or(false)
                })
                .cloned()
                .collect()
        };

        if candidates.is_empty() {
            debug!("No candidates after colo filtering");
            return Vec::new();
        }

        let mut rng = rand::thread_rng();

        // 随机选择 n 个子网
        let selected_subnets: Vec<IpNet> = if candidates.len() <= n_subnets {
            candidates
        } else {
            candidates
                .choose_multiple(&mut rng, n_subnets)
                .cloned()
                .collect()
        };

        // 为每个子网生成 m 个 IP
        let mut target_ips = Vec::with_capacity(selected_subnets.len() * m_ips);
        for subnet in selected_subnets {
            for _ in 0..m_ips {
                target_ips.push(generate_random_ip_in_subnet(&subnet, &mut rng));
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

    /// 获取最佳子网数量
    pub fn best_subnet_count(&self) -> usize {
        self.best_subnets_cache.read().len()
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

    /// 清空所有数据
    pub fn clear(&self) {
        self.subnets.clear();
        self.best_subnets_cache.write().clear();
    }
}

/// 计算 top K 的数量
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
        assert_eq!(manager.best_subnet_count(), 0);
    }
}
