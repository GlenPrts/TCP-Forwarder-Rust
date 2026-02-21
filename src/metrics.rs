use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

/// 转发服务运行时统计（原子操作，无锁）
pub struct ForwardMetrics {
    /// 总连接数（已接受）
    total_connections: AtomicU64,
    /// 当前活跃连接数
    active_connections: AtomicU64,
    /// 竞速连接成功数
    race_successes: AtomicU64,
    /// 竞速连接失败数（含回退）
    race_failures: AtomicU64,
    /// 连接池命中数
    pool_hits: AtomicU64,
    /// 总传输字节数（TX）
    bytes_tx: AtomicU64,
    /// 总传输字节数（RX）
    bytes_rx: AtomicU64,
}

impl ForwardMetrics {
    pub fn new() -> Self {
        Self {
            total_connections: AtomicU64::new(0),
            active_connections: AtomicU64::new(0),
            race_successes: AtomicU64::new(0),
            race_failures: AtomicU64::new(0),
            pool_hits: AtomicU64::new(0),
            bytes_tx: AtomicU64::new(0),
            bytes_rx: AtomicU64::new(0),
        }
    }

    pub fn inc_total_connections(&self) {
        self.total_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_active(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_active(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn inc_race_success(&self) {
        self.race_successes.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_race_failure(&self) {
        self.race_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_pool_hit(&self) {
        self.pool_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_bytes(&self, tx: u64, rx: u64) {
        self.bytes_tx.fetch_add(tx, Ordering::Relaxed);
        self.bytes_rx.fetch_add(rx, Ordering::Relaxed);
    }

    /// 获取统计快照
    pub fn snapshot(&self) -> ForwardMetricsSnapshot {
        ForwardMetricsSnapshot {
            total_connections: self.total_connections.load(Ordering::Relaxed),
            active_connections: self.active_connections.load(Ordering::Relaxed),
            race_successes: self.race_successes.load(Ordering::Relaxed),
            race_failures: self.race_failures.load(Ordering::Relaxed),
            pool_hits: self.pool_hits.load(Ordering::Relaxed),
            bytes_tx: self.bytes_tx.load(Ordering::Relaxed),
            bytes_rx: self.bytes_rx.load(Ordering::Relaxed),
        }
    }
}

/// 转发统计快照（用于序列化输出）
#[derive(Serialize, Clone)]
pub struct ForwardMetricsSnapshot {
    pub total_connections: u64,
    pub active_connections: u64,
    pub race_successes: u64,
    pub race_failures: u64,
    pub pool_hits: u64,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
}
