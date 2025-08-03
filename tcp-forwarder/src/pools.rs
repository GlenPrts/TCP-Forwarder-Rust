use anyhow::Result;
use dashmap::DashMap;
use futures::future::try_join_all;
use socket2::{TcpKeepalive};
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, AtomicU64, Ordering};
use std::time::Instant;
use tokio::net::TcpStream;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, instrument, trace, warn};

use crate::config::{RemotesConfig, DynamicStrategyConfig, ScalingConfig};
use crate::metrics::METRICS;
use crate::scorer::ScoreBoard;
use crate::selector::ActiveRemotes;

/// 动态连接池统计信息
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// 当前活跃连接数（正在使用的连接）
    pub active_connections: Arc<AtomicUsize>,
    /// 当前池中可用连接数
    pub available_connections: Arc<AtomicUsize>,
    /// 总连接数（活跃 + 可用）
    pub total_connections: Arc<AtomicUsize>,
    /// 峰值并发连接数历史记录（用于动态伸缩决策）
    pub peak_concurrency_history: Arc<RwLock<VecDeque<usize>>>,
    /// 最后一次伸缩调整时间
    pub last_scaling_time: Arc<RwLock<Instant>>,
    /// 连接请求计数器（用于计算需求）
    pub connection_requests: Arc<AtomicU64>,
}

impl PoolStats {
    pub fn new() -> Self {
        Self {
            active_connections: Arc::new(AtomicUsize::new(0)),
            available_connections: Arc::new(AtomicUsize::new(0)),
            total_connections: Arc::new(AtomicUsize::new(0)),
            peak_concurrency_history: Arc::new(RwLock::new(VecDeque::new())),
            last_scaling_time: Arc::new(RwLock::new(Instant::now())),
            connection_requests: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 记录连接被获取（变为活跃状态）
    pub async fn record_connection_acquired(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
        self.available_connections.fetch_sub(1, Ordering::Relaxed);
        self.connection_requests.fetch_add(1, Ordering::Relaxed);
        
        // 更新峰值并发历史
        let current_active = self.active_connections.load(Ordering::Relaxed);
        let mut history = self.peak_concurrency_history.write().await;
        
        // 保持最近100个记录
        if history.len() >= 100 {
            history.pop_front();
        }
        history.push_back(current_active);
    }

    /// 记录连接被释放（从活跃变为可用或关闭）
    pub fn record_connection_released(&self, returned_to_pool: bool) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
        if returned_to_pool {
            self.available_connections.fetch_add(1, Ordering::Relaxed);
        } else {
            self.total_connections.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// 记录新连接创建
    pub fn record_connection_created(&self) {
        self.available_connections.fetch_add(1, Ordering::Relaxed);
        self.total_connections.fetch_add(1, Ordering::Relaxed);
    }

    /// 记录连接关闭
    pub fn record_connection_closed(&self) {
        let available = self.available_connections.load(Ordering::Relaxed);
        if available > 0 {
            self.available_connections.fetch_sub(1, Ordering::Relaxed);
        }
        self.total_connections.fetch_sub(1, Ordering::Relaxed);
    }

    /// 获取近期峰值并发数
    pub async fn get_recent_peak_concurrency(&self) -> usize {
        let history = self.peak_concurrency_history.read().await;
        history.iter().cloned().max().unwrap_or(0)
    }

    /// 计算目标连接池大小
    pub async fn calculate_target_size(&self, scaling_config: &ScalingConfig) -> usize {
        let recent_peak = self.get_recent_peak_concurrency().await;
        let buffer_size = (recent_peak as f64 * scaling_config.target_buffer_ratio).ceil() as usize;
        recent_peak + buffer_size
    }
}

/// 连接池中的连接包装器
#[derive(Debug)]
pub struct PooledConnection {
    /// TCP连接
    pub stream: TcpStream,
    /// 连接创建时间
    pub created_at: Instant,
    /// 最后一次活跃时间
    pub last_active: Instant,
}

impl PooledConnection {
    pub fn new(stream: TcpStream) -> Self {
        let now = Instant::now();
        Self {
            stream,
            created_at: now,
            last_active: now,
        }
    }

    /// 更新最后活跃时间
    pub fn touch(&mut self) {
        self.last_active = Instant::now();
    }

    /// 检查连接是否过期
    pub fn is_expired(&self, max_idle_time: Duration) -> bool {
        self.last_active.elapsed() > max_idle_time
    }
}

/// 连接池管理器类型定义
pub type PoolManager = Arc<DashMap<IpAddr, PoolState>>;

/// 单个IP的连接池状态
#[derive(Debug, Clone)]
pub struct PoolState {
    /// 连接池的发送端，filler任务用它向池中添加连接
    pub sender: Sender<PooledConnection>,
    /// 连接池的接收端，用于获取预建立的连接
    pub receiver: Arc<tokio::sync::Mutex<Receiver<PooledConnection>>>,
    /// 池状态标记，用于控制填充任务的停止
    pub is_active: Arc<tokio::sync::RwLock<bool>>,
    /// 动态连接池统计信息
    pub stats: PoolStats,
    /// 动态策略配置
    pub dynamic_config: Option<Arc<DynamicStrategyConfig>>,
}

impl PoolState {
    /// 创建新的连接池状态
    pub fn new(buffer_size: usize, dynamic_config: Option<Arc<DynamicStrategyConfig>>) -> Self {
        let (sender, receiver) = mpsc::channel(buffer_size);
        Self {
            sender,
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
            is_active: Arc::new(tokio::sync::RwLock::new(true)),
            stats: PoolStats::new(),
            dynamic_config,
        }
    }

    /// 标记连接池为非活跃状态
    pub async fn mark_inactive(&self) {
        let mut active = self.is_active.write().await;
        *active = false;
    }

    /// 检查连接池是否活跃
    pub async fn is_active(&self) -> bool {
        *self.is_active.read().await
    }
}

/// 为指定IP创建连接池，包含填充任务和健康检查任务
pub async fn create_pool_for_ip(
    ip: IpAddr,
    port: u16,
    pools: PoolManager,
    pool_size: usize,
    buffer_size: usize,
    dynamic_config: Option<Arc<DynamicStrategyConfig>>,
) {
    let pool_state = PoolState::new(buffer_size, dynamic_config.clone());
    pools.insert(ip, pool_state);

    if let Some(pool_state_ref) = pools.get(&ip) {
        let pool_state = pool_state_ref.value().clone();

        // 启动连接填充任务
        let filler_pool_state = pool_state.clone();
        let filler_dynamic_config = dynamic_config.clone();
        tokio::spawn(async move {
            if let Some(ref config) = filler_dynamic_config {
                dynamic_filler_task(ip, port, filler_pool_state, config.clone()).await;
            } else {
                filler_task(ip, port, filler_pool_state, pool_size).await;
            }
        });

        // 启动健康检查任务
        let health_check_pool_state = pool_state.clone();
        tokio::spawn(async move {
            health_check_task(health_check_pool_state).await;
        });

        // 如果是动态策略，启动动态伸缩任务
        if let Some(ref config) = dynamic_config {
            let scaling_pool_state = pool_state.clone();
            let scaling_config = config.clone();
            tokio::spawn(async move {
                dynamic_scaling_task(ip, port, scaling_pool_state, scaling_config).await;
            });
        }
    }
}

/// 动态连接池伸缩任务
async fn dynamic_scaling_task(
    ip: IpAddr,
    port: u16,
    pool_state: PoolState,
    dynamic_config: Arc<DynamicStrategyConfig>,
) {
    info!("启动动态伸缩任务，IP: {}", ip);
    
    let mut interval = interval(dynamic_config.scaling.interval);
    
    while pool_state.is_active().await {
        interval.tick().await;
        
        // 计算目标连接池大小
        let target_size = pool_state.stats.calculate_target_size(&dynamic_config.scaling).await;
        let current_total = pool_state.stats.total_connections.load(Ordering::Relaxed);
        let current_available = pool_state.stats.available_connections.load(Ordering::Relaxed);
        let current_active = pool_state.stats.active_connections.load(Ordering::Relaxed);
        
        // 应用min/max限制
        let target_size = target_size.clamp(dynamic_config.min_size, dynamic_config.max_size);
        
        debug!(
            "动态伸缩检查 - IP: {}, 当前总数: {}, 可用: {}, 活跃: {}, 目标: {}",
            ip, current_total, current_available, current_active, target_size
        );
        
        // 检查是否需要伸缩
        let mut last_scaling_time = pool_state.stats.last_scaling_time.write().await;
        let time_since_last_scaling = last_scaling_time.elapsed();
        
        // 防止过于频繁的伸缩操作（至少间隔1秒）
        if time_since_last_scaling < Duration::from_secs(1) {
            continue;
        }
        
        if target_size > current_total {
            // 需要扩容
            let scale_up_count = (target_size - current_total).min(dynamic_config.scaling.scale_up_increment);
            if scale_up_count > 0 {
                info!("扩容连接池 - IP: {}, 增加 {} 个连接", ip, scale_up_count);
                scale_up_pool(ip, port, &pool_state, scale_up_count).await;
                *last_scaling_time = Instant::now();
                
                // 记录指标
                METRICS.record_pool_scaled_up(ip.to_string(), scale_up_count);
            }
        } else if target_size < current_total && current_available > dynamic_config.scaling.scale_down_increment {
            // 需要缩容（只有当可用连接数足够时才缩容）
            let scale_down_count = (current_total - target_size).min(dynamic_config.scaling.scale_down_increment);
            if scale_down_count > 0 {
                info!("缩容连接池 - IP: {}, 减少 {} 个连接", ip, scale_down_count);
                scale_down_pool(&pool_state, scale_down_count).await;
                *last_scaling_time = Instant::now();
                
                // 记录指标
                METRICS.record_pool_scaled_down(ip.to_string(), scale_down_count);
            }
        }
        
        // 更新池大小指标
        METRICS.record_pool_statistics(ip.to_string(), current_total, current_available, current_active);
    }
    
    info!("动态伸缩任务停止，IP: {}", ip);
}

/// 扩容连接池
async fn scale_up_pool(ip: IpAddr, port: u16, pool_state: &PoolState, count: usize) {
    let tasks: Vec<_> = (0..count)
        .map(|_| {
            let sender = pool_state.sender.clone();
            let stats = pool_state.stats.clone();
            tokio::spawn(async move {
                match create_connection_with_keepalive(ip, port).await {
                    Ok(stream) => {
                        let connection = PooledConnection::new(stream);
                        if sender.send(connection).await.is_ok() {
                            stats.record_connection_created();
                            METRICS.record_pool_connection_created();
                            debug!("扩容：成功向连接池添加新连接到 {}", ip);
                        } else {
                            debug!("扩容：连接池通道已关闭，无法添加连接到 {}", ip);
                        }
                    }
                    Err(e) => {
                        warn!("扩容：创建到 {} 的连接失败: {}", ip, e);
                        METRICS.record_pool_connection_failed();
                    }
                }
            })
        })
        .collect();

    // 等待所有连接创建任务完成，设置超时
    let timeout_duration = Duration::from_secs(5);
    match tokio::time::timeout(timeout_duration, futures::future::join_all(tasks)).await {
        Ok(_) => {
            debug!("扩容任务完成，IP: {}", ip);
        }
        Err(_) => {
            warn!("扩容任务超时，IP: {}", ip);
        }
    }
}

/// 缩容连接池
async fn scale_down_pool(pool_state: &PoolState, count: usize) {
    let mut receiver = pool_state.receiver.lock().await;
    let mut removed_count = 0;
    
    // 移除指定数量的空闲连接
    for _ in 0..count {
        if receiver.try_recv().is_ok() {
            pool_state.stats.record_connection_closed();
            METRICS.record_pool_connection_closed();
            removed_count += 1;
        } else {
            break; // 没有更多空闲连接可移除
        }
    }
    
    debug!("缩容：实际移除了 {} 个连接", removed_count);
}

/// 动态连接池填充任务，根据需求动态调整
async fn dynamic_filler_task(
    ip: IpAddr,
    port: u16,
    pool_state: PoolState,
    dynamic_config: Arc<DynamicStrategyConfig>,
) {
    info!("启动动态填充任务，IP: {}", ip);
    
    let check_interval = Duration::from_millis(500); // 更频繁的检查
    let mut interval = interval(check_interval);

    while pool_state.is_active().await {
        interval.tick().await;

        let current_available = pool_state.stats.available_connections.load(Ordering::Relaxed);
        let current_total = pool_state.stats.total_connections.load(Ordering::Relaxed);
        
        // 确保至少有最小数量的连接
        let min_needed = dynamic_config.min_size.saturating_sub(current_total);
        
        // 检查可用连接是否不足
        let available_threshold = (current_total / 4).max(2); // 至少保持25%或2个可用连接
        let availability_needed = if current_available < available_threshold {
            available_threshold - current_available
        } else {
            0
        };
        
        let needed = min_needed.max(availability_needed);
        
        if needed > 0 && current_total < dynamic_config.max_size {
            let create_count = needed.min(dynamic_config.max_size - current_total);
            debug!("动态填充需要补充 {} 个连接到 {}", create_count, ip);

            // 创建连接但不超过最大限制
            let tasks: Vec<_> = (0..create_count)
                .map(|_| {
                    let sender = pool_state.sender.clone();
                    let stats = pool_state.stats.clone();
                    tokio::spawn(async move {
                        match create_connection_with_keepalive(ip, port).await {
                            Ok(stream) => {
                                let connection = PooledConnection::new(stream);
                                if sender.send(connection).await.is_ok() {
                                    stats.record_connection_created();
                                    METRICS.record_pool_connection_created();
                                    debug!("动态填充：成功向连接池添加新连接到 {}", ip);
                                } else {
                                    debug!("动态填充：连接池已满，无法添加连接到 {}", ip);
                                }
                            }
                            Err(e) => {
                                warn!("动态填充：创建到 {} 的连接失败: {}", ip, e);
                                METRICS.record_pool_connection_failed();
                            }
                        }
                    })
                })
                .collect();

            // 等待连接创建完成，设置超时
            let timeout_duration = Duration::from_secs(3);
            match tokio::time::timeout(timeout_duration, futures::future::join_all(tasks)).await {
                Ok(_) => {
                    debug!("动态填充任务完成，IP: {}", ip);
                }
                Err(_) => {
                    warn!("动态填充任务超时，IP: {}", ip);
                }
            }
        } else {
            trace!(
                "动态填充检查 - IP: {} 连接充足: 总数={}/{}, 可用={}, 阈值={}",
                ip, current_total, dynamic_config.max_size, current_available, available_threshold
            );
        }
    }

    info!("动态填充任务停止，IP: {}", ip);
}

/// 连接池健康检查任务
async fn health_check_task(pool_state: PoolState) {
    const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(30);
    const MAX_IDLE_TIME: Duration = Duration::from_secs(120);

    let mut interval = interval(HEALTH_CHECK_INTERVAL);

    while pool_state.is_active().await {
        interval.tick().await;

        let mut receiver = pool_state.receiver.lock().await;
        let mut valid_connections = Vec::new();

        // 取出所有连接进行检查
        while let Ok(mut connection) = receiver.try_recv() {
            if !connection.is_expired(MAX_IDLE_TIME) {
                // 检查连接是否还有效（简单的可读性检查）
                if is_connection_healthy(&mut connection.stream).await {
                    connection.touch();
                    valid_connections.push(connection);
                    METRICS.record_pool_health_check(true);
                } else {
                    info!("从连接池中移除无效连接");
                    METRICS.record_pool_health_check(false);
                    METRICS.record_pool_connection_closed();
                }
            } else {
                info!("从连接池中移除过期连接");
                METRICS.record_pool_health_check(false);
                METRICS.record_pool_connection_closed();
            }
        }

        // 将有效连接放回池中
        for connection in valid_connections {
            if pool_state.sender.try_send(connection).is_err() {
                // 如果放不回去说明池已满，这是正常情况
                break;
            }
        }

        drop(receiver);
    }
}

/// 检查TCP连接是否健康
async fn is_connection_healthy(stream: &mut TcpStream) -> bool {
    // 使用非阻塞方式检查连接状态
    stream
        .ready(tokio::io::Interest::READABLE | tokio::io::Interest::WRITABLE)
        .await
        .is_ok()
}

/// 连接池填充任务，负责维护池中的连接数量
async fn filler_task(ip: IpAddr, port: u16, pool_state: PoolState, target_size: usize) {
    let check_interval = Duration::from_secs(5);
    let mut interval = interval(check_interval);

    while pool_state.is_active().await {
        interval.tick().await;

        // 检查当前池中连接数量
        let receiver = pool_state.receiver.lock().await;
        let current_count = receiver.len();
        drop(receiver);

        let needed = target_size.saturating_sub(current_count);
        if needed > 0 {
            debug!("连接池需要补充 {} 个连接到 {}", needed, ip);

            // 并行创建多个连接以提高效率
            let tasks: Vec<_> = (0..needed)
                .map(|_| {
                    let sender = pool_state.sender.clone();
                    tokio::spawn(async move {
                        match create_connection_with_keepalive(ip, port).await {
                            Ok(stream) => {
                                let connection = PooledConnection::new(stream);
                                if sender.send(connection).await.is_ok() {
                                    METRICS.record_pool_connection_created();
                                    debug!("成功向连接池添加新连接到 {}", ip);
                                } else {
                                    debug!("连接池已满，无法添加连接到 {}", ip);
                                }
                            }
                            Err(e) => {
                                warn!("创建到 {} 的连接失败: {}", ip, e);
                                METRICS.record_pool_connection_failed();
                            }
                        }
                    })
                })
                .collect();

            // 等待所有连接创建任务完成，但设置超时
            let timeout_duration = Duration::from_secs(10);
            match tokio::time::timeout(
                timeout_duration,
                futures::future::join_all(tasks),
            )
            .await
            {
                Ok(_) => {
                    debug!("连接池填充任务完成，目标IP: {}", ip);
                }
                Err(_) => {
                    warn!("连接池填充任务超时，目标IP: {}", ip);
                }
            }
        } else {
            trace!("连接池 {} 连接充足: {}/{}", ip, current_count, target_size);
        }
    }

    info!("连接池填充任务停止，IP: {}", ip);
}

/// 创建带TCP keepalive的连接
pub async fn create_connection_with_keepalive(ip: IpAddr, port: u16) -> anyhow::Result<TcpStream> {
    let addr = SocketAddr::new(ip, port);

    // 先创建标准连接
    let stream = TcpStream::connect(addr).await?;

    // 获取底层socket并设置keepalive
    let socket = socket2::Socket::from(stream.into_std()?);
    
    // 配置TCP keepalive
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(60)) // 60秒后开始发送keepalive
        .with_interval(Duration::from_secs(10)) // 每10秒发送一次keepalive
        .with_retries(3); // 重试3次

    socket.set_tcp_keepalive(&keepalive)?;
    
    // 转换回tokio TcpStream
    socket.set_nonblocking(true)?;
    let std_stream: std::net::TcpStream = socket.into();
    let stream = TcpStream::from_std(std_stream)?;

    debug!("成功创建带keepalive的连接到 {}", addr);
    Ok(stream)
}

/// 从连接池获取一个连接
///
/// 如果池中有可用连接，立即返回；否则返回None
/// 会验证连接的有效性，如果连接已关闭会自动丢弃
pub async fn get_connection_from_pool(
    pool_manager: &PoolManager,
    ip: IpAddr,
) -> Option<PooledConnection> {
    if let Some(pool_state) = pool_manager.get(&ip) {
        let mut receiver = pool_state.receiver.lock().await;

        // 尝试从池中获取连接
        while let Ok(mut connection) = receiver.try_recv() {
            // 检查连接是否仍然有效
            if is_connection_healthy(&mut connection.stream).await {
                connection.touch();
                
                // 更新统计信息
                pool_state.stats.record_connection_acquired().await;
                METRICS.record_pool_connection_reused();
                debug!("从连接池获取到有效连接: {}", ip);
                return Some(connection);
            } else {
                // 连接无效，记录并继续尝试下一个
                pool_state.stats.record_connection_closed();
                METRICS.record_pool_connection_closed();
                debug!("连接池中发现无效连接，已丢弃: {}", ip);
            }
        }
    }

    None
}

/// 将连接返回到连接池
pub async fn return_connection_to_pool(
    pool_manager: &PoolManager,
    ip: IpAddr,
    connection: PooledConnection,
) -> bool {
    if let Some(pool_state) = pool_manager.get(&ip) {
        // 检查连接是否仍然健康
        let mut conn = connection;
        if is_connection_healthy(&mut conn.stream).await {
            conn.touch();
            
            // 尝试将连接放回池中
            if pool_state.sender.try_send(conn).is_ok() {
                pool_state.stats.record_connection_released(true);
                debug!("连接成功返回到连接池: {}", ip);
                return true;
            } else {
                // 池已满，丢弃连接
                pool_state.stats.record_connection_released(false);
                debug!("连接池已满，丢弃连接: {}", ip);
            }
        } else {
            // 连接不健康，直接丢弃
            pool_state.stats.record_connection_released(false);
            debug!("连接不健康，丢弃连接: {}", ip);
        }
    }
    
    false
}

/// 获取连接池统计信息
pub async fn get_pool_statistics(pool_manager: &PoolManager, ip: IpAddr) -> Option<(usize, usize, usize, usize)> {
    if let Some(pool_state) = pool_manager.get(&ip) {
        let total = pool_state.stats.total_connections.load(Ordering::Relaxed);
        let available = pool_state.stats.available_connections.load(Ordering::Relaxed);
        let active = pool_state.stats.active_connections.load(Ordering::Relaxed);
        let recent_peak = pool_state.stats.get_recent_peak_concurrency().await;
        
        Some((total, available, active, recent_peak))
    } else {
        None
    }
}

/// 创建连接池管理器
pub fn create_pool_manager() -> PoolManager {
    Arc::new(DashMap::new())
}

/// 连接池管理任务
pub async fn pool_manager_task(
    pool_manager: PoolManager,
    active_remotes: ActiveRemotes,
    _score_board: ScoreBoard,
    remotes_config: Arc<crate::config::RemotesConfig>,
    pools_config: Arc<crate::config::PoolsConfig>,
) -> Result<()> {
    info!("启动连接池管理任务");

    let mut check_interval = tokio::time::interval(Duration::from_secs(10));
    let mut last_known_ips = std::collections::HashSet::new();

    loop {
        check_interval.tick().await;

        // 获取当前活跃的IP列表
        let current_ips = {
            let active_ips = active_remotes.read().await;
            active_ips.iter().cloned().collect::<std::collections::HashSet<_>>()
        };

        // 检查是否有新的IP需要创建连接池
        for ip in &current_ips {
            if !last_known_ips.contains(ip) && !pool_manager.contains_key(ip) {
                info!("为新的活跃IP {} 创建连接池", ip);
                
                // 根据策略配置决定池参数
                let (pool_size, buffer_size, dynamic_config) = match pools_config.strategy.strategy_type.as_str() {
                    "static" => {
                        let size = pools_config.strategy.static_.as_ref()
                            .map(|s| s.size_per_remote)
                            .unwrap_or(50);
                        (size, size * 2, None)
                    }
                    "dynamic" => {
                        let config = pools_config.strategy.dynamic.as_ref()
                            .expect("动态策略配置缺失");
                        let buffer_size = config.max_size.max(100); // 确保足够的缓冲区
                        (config.min_size, buffer_size, Some(Arc::new(config.clone())))
                    }
                    _ => {
                        warn!("未知的连接池策略: {}, 使用默认静态策略", pools_config.strategy.strategy_type);
                        (50, 100, None)
                    }
                };
                
                create_pool_for_ip(
                    *ip,
                    remotes_config.default_remote_port,
                    pool_manager.clone(),
                    pool_size,
                    buffer_size,
                    dynamic_config,
                ).await;
            }
        }

        // 检查是否有IP不再活跃，需要清理连接池
        for ip in &last_known_ips {
            if !current_ips.contains(ip) {
                if let Some((_, pool_state)) = pool_manager.remove(ip) {
                    info!("IP {} 不再活跃，停止其连接池", ip);
                    pool_state.mark_inactive().await;
                }
            }
        }

        // 更新已知IP列表
        last_known_ips = current_ips;

        // 输出统计信息
        if !last_known_ips.is_empty() {
            debug!("连接池管理任务心跳，当前活跃IP数量: {}", last_known_ips.len());
            
            // 定期输出详细统计信息
            for ip in &last_known_ips {
                if let Some(stats) = get_pool_statistics(&pool_manager, *ip).await {
                    debug!(
                        "连接池统计 - IP: {}, 总数: {}, 可用: {}, 活跃: {}, 近期峰值: {}",
                        ip, stats.0, stats.1, stats.2, stats.3
                    );
                }
            }
        }
    }
}

/// 创建并管理所有远程IP的连接池
#[instrument(skip(active_remotes, _score_board, pools_config))]
pub async fn initialize_pools(
    remotes_config: &RemotesConfig,
    active_remotes: ActiveRemotes,
    _score_board: ScoreBoard,
    pools_config: &crate::config::PoolsConfig,
) -> Result<PoolManager> {
    let pool_manager = Arc::new(DashMap::new());

    // 从active_remotes获取当前活跃的IP列表
    let active_ips = active_remotes.read().await;
    let ip_count = active_ips.len();
    info!("开始初始化连接池，远程数量: {}, 策略: {}", ip_count, pools_config.strategy.strategy_type);

    // 根据策略配置决定池参数
    let (pool_size, buffer_size, dynamic_config) = match pools_config.strategy.strategy_type.as_str() {
        "static" => {
            let size = pools_config.strategy.static_.as_ref()
                .map(|s| s.size_per_remote)
                .unwrap_or(50);
            info!("使用静态连接池策略，每个IP连接数: {}", size);
            (size, size * 2, None)
        }
        "dynamic" => {
            let config = pools_config.strategy.dynamic.as_ref()
                .expect("动态策略配置缺失");
            let buffer_size = config.max_size.max(100);
            info!(
                "使用动态连接池策略，范围: {}-{}, 缓冲比例: {:.1}%",
                config.min_size, config.max_size, config.scaling.target_buffer_ratio * 100.0
            );
            (config.min_size, buffer_size, Some(Arc::new(config.clone())))
        }
        _ => {
            warn!("未知的连接池策略: {}, 使用默认静态策略", pools_config.strategy.strategy_type);
            (50, 100, None)
        }
    };

    // 为每个远程IP创建连接池
    let pool_creation_tasks: Vec<_> = active_ips
        .iter()
        .map(|ip| {
            let pools = pool_manager.clone();
            let ip = *ip;
            let port = remotes_config.default_remote_port;
            let dynamic_config_clone = dynamic_config.clone();

            tokio::spawn(async move {
                create_pool_for_ip(ip, port, pools, pool_size, buffer_size, dynamic_config_clone).await;
            })
        })
        .collect();

    // 等待所有连接池创建完成
    if let Err(e) = try_join_all(pool_creation_tasks).await {
        error!("创建连接池失败: {}", e);
        return Err(anyhow::anyhow!("连接池初始化失败: {}", e));
    }

    info!("连接池初始化完成，策略: {}", pools_config.strategy.strategy_type);
    Ok(pool_manager)
}

/// 停止所有连接池
pub async fn shutdown_pools(pool_manager: &PoolManager) {
    info!("开始停止所有连接池");

    for pool_entry in pool_manager.iter() {
        let ip = *pool_entry.key();
        let pool_state = pool_entry.value();

        info!("停止连接池: {}", ip);
        pool_state.mark_inactive().await;
    }

    info!("所有连接池已停止");
}

/// 检查连接是否活跃
async fn is_connection_alive(stream: &TcpStream) -> bool {
    // 尝试读取一个字节但不移除它，如果连接关闭会立即返回错误
    match stream.ready(tokio::io::Interest::READABLE).await {
        Ok(_) => {
            // 连接可读，但这可能意味着有数据或连接已关闭
            // 我们需要尝试实际读取来确定
            let mut buf = [0u8; 1];
            match stream.try_read(&mut buf) {
                Ok(0) => false,  // 读取到0字节表示连接已关闭
                Ok(_) => {
                    // 读取到数据，但这在我们的场景下不应该发生
                    // 因为我们期望的是空闲连接
                    true
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // 没有数据可读，连接仍然活跃
                    true
                }
                Err(_) => false, // 其他错误表示连接有问题
            }
        }
        Err(_) => false, // 连接不可用
    }
}
