use crate::config::{AppConfig, ConnectionPoolConfig};
use crate::state::IpManager;
use socket2::{SockRef, TcpKeepalive};
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

/// 单个预连接的超时时间（毫秒）
const PRE_CONNECT_TIMEOUT_MS: u64 = 2000;
/// TCP Keepalive 空闲时间（秒）
const KEEPALIVE_TIME_SECS: u64 = 60;
/// TCP Keepalive 探测间隔（秒）
const KEEPALIVE_INTERVAL_SECS: u64 = 10;

/// 池化连接包装
struct PooledConnection {
    stream: TcpStream,
    ip: IpAddr,
    created_at: Instant,
}

/// 连接池统计信息（原子操作，无锁）
pub struct PoolStats {
    total_acquired: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    /// 因超过 max_idle_secs 过期而丢弃
    expired: AtomicU64,
    /// 因对端关闭/RST 而丢弃
    dead: AtomicU64,
    /// push 时池满被挤出的连接
    evicted: AtomicU64,
    /// 后台清理移除的连接
    cleaned: AtomicU64,
}

impl PoolStats {
    fn new() -> Self {
        Self {
            total_acquired: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            expired: AtomicU64::new(0),
            dead: AtomicU64::new(0),
            evicted: AtomicU64::new(0),
            cleaned: AtomicU64::new(0),
        }
    }
}

/// 连接池统计快照（用于序列化输出）
#[derive(serde::Serialize, Clone)]
pub struct PoolStatsSnapshot {
    pub pool_size: usize,
    pub total_acquired: u64,
    pub hits: u64,
    pub misses: u64,
    pub expired: u64,
    pub dead: u64,
    pub evicted: u64,
    pub cleaned: u64,
}

/// 预连接池
///
/// 提前建立 TCP 连接，消除客户端请求时的握手延迟。
/// 池空时无缝降级到竞速连接模式。
pub struct ConnectionPool {
    conns: Mutex<VecDeque<PooledConnection>>,
    stats: PoolStats,
    /// acquire 消费后通知后台立即补充
    refill_notify: Notify,
    config: ConnectionPoolConfig,
}

impl ConnectionPool {
    /// 创建预连接池
    ///
    /// # 参数
    /// - `config`: 连接池配置
    ///
    /// # 返回值
    /// 新的连接池实例
    pub fn new(config: ConnectionPoolConfig) -> Self {
        Self {
            conns: Mutex::new(VecDeque::with_capacity(config.pool_size)),
            stats: PoolStats::new(),
            refill_notify: Notify::new(),
            config,
        }
    }

    /// 从池中获取一个存活的连接
    ///
    /// 使用 LIFO 策略（最新连接优先），存活概率更高。
    /// 取出时检查创建时间 + poll_read 探测双重验证。
    ///
    /// # 返回值
    /// - `Some((TcpStream, IpAddr))`: 存活连接及其 IP
    /// - `None`: 池为空或所有连接已失效
    pub async fn acquire(&self) -> Option<(TcpStream, IpAddr)> {
        self.stats.total_acquired.fetch_add(1, Ordering::Relaxed);

        loop {
            let mut conn = {
                let mut queue = self.conns.lock().await;
                let conn = queue.pop_back();
                if conn.is_none() {
                    self.stats.misses.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
                conn.unwrap()
            };

            if !self.is_conn_fresh(&conn) {
                self.stats.expired.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            if !is_stream_alive(&mut conn.stream) {
                self.stats.dead.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            self.stats.hits.fetch_add(1, Ordering::Relaxed);
            debug!("Pool hit: reusing connection to {}", conn.ip);
            self.refill_notify.notify_one();
            return Some((conn.stream, conn.ip));
        }
    }

    /// 获取连接池统计快照
    pub async fn snapshot(&self) -> PoolStatsSnapshot {
        let queue = self.conns.lock().await;
        PoolStatsSnapshot {
            pool_size: queue.len(),
            total_acquired: self.stats.total_acquired.load(Ordering::Relaxed),
            hits: self.stats.hits.load(Ordering::Relaxed),
            misses: self.stats.misses.load(Ordering::Relaxed),
            expired: self.stats.expired.load(Ordering::Relaxed),
            dead: self.stats.dead.load(Ordering::Relaxed),
            evicted: self.stats.evicted.load(Ordering::Relaxed),
            cleaned: self.stats.cleaned.load(Ordering::Relaxed),
        }
    }

    /// 获取当前池中连接数
    async fn current_size(&self) -> usize {
        self.conns.lock().await.len()
    }

    /// 检查连接是否在有效期内
    fn is_conn_fresh(&self, conn: &PooledConnection) -> bool {
        let max_idle = Duration::from_secs(self.config.max_idle_secs);
        conn.created_at.elapsed() < max_idle
    }

    /// 将新建立的连接放入池中
    ///
    /// 如果池已满则丢弃最老的连接。
    async fn push(&self, conn: PooledConnection) {
        let mut queue = self.conns.lock().await;
        if queue.len() >= self.config.pool_size {
            queue.pop_front();
            self.stats.evicted.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back(conn);
    }

    /// 主动清理池中过期和死亡的连接
    ///
    /// 从队列头部（最老的）开始扫描，移除所有不健康连接。
    /// 在 refill 循环中调用，保持池内连接质量。
    ///
    /// # 返回值
    /// 本次清理移除的连接数
    async fn purge_stale(&self) -> usize {
        let mut items = {
            let mut queue = self.conns.lock().await;
            if queue.is_empty() {
                return 0;
            }
            std::mem::take(&mut *queue)
        };

        let before = items.len();
        let mut expired_count = 0u64;
        let mut dead_count = 0u64;

        items.retain_mut(|conn| {
            if !self.is_conn_fresh(conn) {
                expired_count += 1;
                return false;
            }
            if !is_stream_alive(&mut conn.stream) {
                dead_count += 1;
                return false;
            }
            true
        });

        let mut queue = self.conns.lock().await;
        let to_keep = items.len();

        // 如果放回会导致超过 pool_size，则丢弃最旧的（即 items 的头部）
        while items.len() + queue.len() > self.config.pool_size 
            && !items.is_empty() 
        {
            items.pop_front();
            self.stats.evicted.fetch_add(1, Ordering::Relaxed);
        }

        // 将 items 插入到 queue 的前端（保持 LIFO，旧连接在前）
        for conn in items.into_iter().rev() {
            queue.push_front(conn);
        }

        if expired_count > 0 {
            self.stats.expired.fetch_add(expired_count, Ordering::Relaxed);
        }
        if dead_count > 0 {
            self.stats.dead.fetch_add(dead_count, Ordering::Relaxed);
        }

        let removed = before - to_keep;
        if removed > 0 {
            self.stats
                .cleaned
                .fetch_add(removed as u64, Ordering::Relaxed);
        }
        removed
    }
}

/// 非阻塞检测 TcpStream 是否存活
///
/// 使用 poll_read + noop_waker 做零开销探测：
/// - Pending → 无数据可读，连接正常
/// - Ready(Ok(0)) → 对端发送 FIN，连接关闭
/// - Ready(Err(_)) → IO 错误，连接已坏
/// - Ready(Ok(n>0)) → 异常数据（空闲连接不该收到）
fn is_stream_alive(stream: &mut TcpStream) -> bool {
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut buf = [0u8; 1];
    let mut read_buf = ReadBuf::new(&mut buf);
    matches!(
        Pin::new(stream).poll_read(&mut cx, &mut read_buf),
        Poll::Pending
    )
}

/// 配置 TCP Keepalive
fn configure_keepalive(stream: &TcpStream) -> std::io::Result<()> {
    let socket_ref = SockRef::from(stream);
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(KEEPALIVE_TIME_SECS))
        .with_interval(Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
    socket_ref.set_tcp_keepalive(&keepalive)
}

/// 建立单个预连接
///
/// # 参数
/// - `ip`: 目标 IP
/// - `port`: 目标端口
/// - `ip_manager`: IP 管理器
///
/// # 返回值
/// 成功返回池化连接，失败返回 None
async fn pre_connect_one(
    ip: IpAddr,
    port: u16,
    ip_manager: &IpManager,
) -> Option<PooledConnection> {
    let _permit = ip_manager.acquire_fd_permit().await.ok();
    let addr = SocketAddr::new(ip, port);
    let connect_result = timeout(
        Duration::from_millis(PRE_CONNECT_TIMEOUT_MS),
        TcpStream::connect(addr),
    )
    .await;

    match connect_result {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            let _ = configure_keepalive(&stream);
            Some(PooledConnection {
                stream,
                ip,
                created_at: Instant::now(),
            })
        }
        Ok(Err(e)) => {
            debug!("Pre-connect to {} failed: {}", addr, e);
            None
        }
        Err(_) => {
            debug!("Pre-connect to {} timed out", addr);
            None
        }
    }
}

/// 补充连接到指定目标水位
///
/// # 参数
/// - `pool`: 连接池
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
/// - `target`: 目标连接数（min_idle 或 pool_size）
///
/// # 返回值
/// 本次成功补充的连接数
async fn refill_to_target(
    pool: &ConnectionPool,
    config: &AppConfig,
    ip_manager: &IpManager,
    target: usize,
) -> usize {
    let current = pool.current_size().await;
    let limit = target.min(pool.config.pool_size);

    // 卫语句：已达到目标水位
    if current >= limit {
        return 0;
    }

    let need = limit - current;
    let candidates = ip_manager.get_target_ips(
        &config.target_colos,
        need,
        1,
    );

    // 卫语句：无可用候选 IP
    if candidates.is_empty() {
        debug!("No candidate IPs for pool refill");
        return 0;
    }

    execute_refill(pool, config.target_port, ip_manager, candidates).await
}

/// 执行并发补充逻辑
///
/// # 参数
/// - `pool`: 连接池
/// - `port`: 目标端口
/// - `ip_manager`: IP 管理器
/// - `candidates`: 候选 IP 列表
///
/// # 返回值
/// 成功建立并放入池中的连接数
async fn execute_refill(
    pool: &ConnectionPool,
    port: u16,
    ip_manager: &IpManager,
    candidates: Vec<IpAddr>,
) -> usize {
    use futures::stream::{FuturesUnordered, StreamExt};
    let mut futures = FuturesUnordered::new();

    for ip in candidates {
        futures.push(pre_connect_one(ip, port, ip_manager));
    }

    let mut filled = 0;
    while let Some(maybe_conn) = futures.next().await {
        add_conn_to_pool(pool, maybe_conn, &mut filled).await;
    }
    filled
}

/// 将建立成功的连接加入池中
///
/// # 参数
/// - `pool`: 连接池
/// - `maybe_conn`: 可能成功的连接
/// - `filled`: 已填充计数器
async fn add_conn_to_pool(
    pool: &ConnectionPool,
    maybe_conn: Option<PooledConnection>,
    filled: &mut usize,
) {
    // 卫语句：连接建立失败
    if maybe_conn.is_none() {
        return;
    }

    let conn = maybe_conn.unwrap();
    debug!("Pool refill: connected to {}", conn.ip);
    pool.push(conn).await;
    *filled += 1;
}

/// 响应式后台连接管理循环
///
/// 两种补充触发方式：
/// 1. acquire 消费连接后通过 Notify 立即补充 1 个
/// 2. 定时清理死连接 + 维持 min_idle 水位
///
/// # 参数
/// - `pool`: 连接池（Arc 共享）
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
/// - `cancel_token`: 取消令牌
pub async fn run_refill_loop(
    pool: Arc<ConnectionPool>,
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    cancel_token: CancellationToken,
) {
    let interval = Duration::from_millis(pool.config.refill_interval_ms);
    let min_idle = pool.config.min_idle;

    info!(
        "Connection pool started \
         (max={}, min_idle={}, idle={}s, interval={}ms)",
        pool.config.pool_size,
        min_idle,
        pool.config.max_idle_secs,
        pool.config.refill_interval_ms,
    );

    // 启动时先填充到 min_idle
    let filled = refill_to_target(&pool, &config, &ip_manager, min_idle).await;
    if filled > 0 {
        debug!("Pool initial fill: {} connections", filled);
    }

    loop {
        tokio::select! {
            biased;
            _ = cancel_token.cancelled() => {
                info!("Pool refill loop shutting down");
                break;
            }
            // acquire 消费后立即补充 1 个
            _ = pool.refill_notify.notified() => {
                let filled = refill_to_target(
                    &pool, &config, &ip_manager,
                    pool.current_size().await + 1,
                ).await;
                if filled > 0 {
                    debug!(
                        "Pool reactive refill: +{} (total: {})",
                        filled,
                        pool.current_size().await,
                    );
                }
            }
            // 定时清理 + 维持 min_idle 水位
            _ = tokio::time::sleep(interval) => {
                let purged = pool.purge_stale().await;
                if purged > 0 {
                    debug!(
                        "Pool purged {} stale connections",
                        purged,
                    );
                }
                let filled = refill_to_target(
                    &pool, &config, &ip_manager, min_idle,
                ).await;
                if filled > 0 {
                    debug!(
                        "Pool periodic refill: +{} (total: {})",
                        filled,
                        pool.current_size().await,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn create_test_conn(
        ip: &str,
        server_addr: SocketAddr,
    ) -> PooledConnection {
        let stream = TcpStream::connect(server_addr).await.unwrap();
        PooledConnection {
            stream,
            ip: ip.parse().unwrap(),
            created_at: Instant::now(),
        }
    }

    async fn start_test_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut _conns = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                _conns.push(stream);
            }
        });
        addr
    }

    #[tokio::test]
    async fn test_pool_lifo() {
        let addr = start_test_server().await;
        let config = ConnectionPoolConfig {
            enabled: true,
            pool_size: 3,
            min_idle: 1,
            max_idle_secs: 10,
            refill_interval_ms: 1000,
        };
        let pool = ConnectionPool::new(config);

        pool.push(create_test_conn("1.1.1.1", addr).await).await;
        pool.push(create_test_conn("2.2.2.2", addr).await).await;
        pool.push(create_test_conn("3.3.3.3", addr).await).await;

        // LIFO: 应该先拿到 3.3.3.3
        let (_, ip) = pool.acquire().await.expect("Should acquire 3.3.3.3");
        assert_eq!(ip, "3.3.3.3".parse::<IpAddr>().unwrap());

        let (_, ip) = pool.acquire().await.expect("Should acquire 2.2.2.2");
        assert_eq!(ip, "2.2.2.2".parse::<IpAddr>().unwrap());

        let (_, ip) = pool.acquire().await.expect("Should acquire 1.1.1.1");
        assert_eq!(ip, "1.1.1.1".parse::<IpAddr>().unwrap());

        assert!(pool.acquire().await.is_none());
    }

    #[tokio::test]
    async fn test_pool_eviction() {
        let addr = start_test_server().await;
        let config = ConnectionPoolConfig {
            enabled: true,
            pool_size: 2,
            min_idle: 1,
            max_idle_secs: 10,
            refill_interval_ms: 1000,
        };
        let pool = ConnectionPool::new(config);

        pool.push(create_test_conn("1.1.1.1", addr).await).await;
        pool.push(create_test_conn("2.2.2.2", addr).await).await;
        // 1.1.1.1 应该被挤出
        pool.push(create_test_conn("3.3.3.3", addr).await).await;

        let stats = pool.snapshot().await;
        assert_eq!(stats.evicted, 1);

        let (_, ip) = pool.acquire().await.expect("Should acquire 3.3.3.3");
        assert_eq!(ip, "3.3.3.3".parse::<IpAddr>().unwrap());

        let (_, ip) = pool.acquire().await.expect("Should acquire 2.2.2.2");
        assert_eq!(ip, "2.2.2.2".parse::<IpAddr>().unwrap());

        assert!(pool.acquire().await.is_none());
    }

    #[tokio::test]
    async fn test_pool_purge_stale() {
        let addr = start_test_server().await;
        let config = ConnectionPoolConfig {
            enabled: true,
            pool_size: 5,
            min_idle: 1,
            max_idle_secs: 1, // 1秒过期
            refill_interval_ms: 1000,
        };
        let pool = ConnectionPool::new(config);

        pool.push(create_test_conn("1.1.1.1", addr).await).await;
        
        let mut conn2 = create_test_conn("2.2.2.2", addr).await;
        // 手动设置过期时间
        conn2.created_at = Instant::now() - Duration::from_secs(2);
        pool.push(conn2).await;

        pool.push(create_test_conn("3.3.3.3", addr).await).await;

        // 清理前有 3 个
        assert_eq!(pool.current_size().await, 3);

        let purged = pool.purge_stale().await;
        assert_eq!(purged, 1); // 2.2.2.2 应该被清理

        // 清理后剩下 2 个，且顺序正确 (LIFO: 3.3.3.3, 1.1.1.1)
        assert_eq!(pool.current_size().await, 2);

        let (_, ip) = pool.acquire().await.expect("Should acquire 3.3.3.3");
        assert_eq!(ip, "3.3.3.3".parse::<IpAddr>().unwrap());

        let (_, ip) = pool.acquire().await.expect("Should acquire 1.1.1.1");
        assert_eq!(ip, "1.1.1.1".parse::<IpAddr>().unwrap());
    }
}

