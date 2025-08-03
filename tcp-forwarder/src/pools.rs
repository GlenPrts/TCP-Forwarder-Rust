use anyhow::Result;
use dashmap::DashMap;
use futures::future::try_join_all;
use socket2::{TcpKeepalive};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpStream;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, instrument, trace, warn};

use crate::config::RemotesConfig;
use crate::metrics::METRICS;
use crate::scorer::ScoreBoard;
use crate::selector::ActiveRemotes;

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
}

impl PoolState {
    /// 创建新的连接池状态
    pub fn new(buffer_size: usize) -> Self {
        let (sender, receiver) = mpsc::channel(buffer_size);
        Self {
            sender,
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
            is_active: Arc::new(tokio::sync::RwLock::new(true)),
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
) {
    let pool_state = PoolState::new(buffer_size);
    pools.insert(ip, pool_state);

    if let Some(pool_state_ref) = pools.get(&ip) {
        let pool_state = pool_state_ref.value().clone();

        // 启动连接填充任务
        let filler_pool_state = pool_state.clone();
        tokio::spawn(async move {
            filler_task(ip, port, filler_pool_state, pool_size).await;
        });

        // 启动健康检查任务
        let health_check_pool_state = pool_state.clone();
        tokio::spawn(async move {
            health_check_task(health_check_pool_state).await;
        });
    }
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
                METRICS.record_pool_connection_reused();
                debug!("从连接池获取到有效连接: {}", ip);
                return Some(connection);
            } else {
                // 连接无效，记录并继续尝试下一个
                METRICS.record_pool_connection_closed();
                debug!("连接池中发现无效连接，已丢弃: {}", ip);
            }
        }
    }

    None
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
    _pools_config: Arc<crate::config::PoolsConfig>,
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
                create_pool_for_ip(
                    *ip,
                    remotes_config.default_remote_port,
                    pool_manager.clone(),
                    5, // 默认池大小
                    10, // 默认缓冲区大小
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

        debug!("连接池管理任务心跳，当前活跃IP数量: {}", last_known_ips.len());
    }
}

/// 创建并管理所有远程IP的连接池
#[instrument(skip(active_remotes, _score_board))]
pub async fn initialize_pools(
    remotes_config: &RemotesConfig,
    active_remotes: ActiveRemotes,
    _score_board: ScoreBoard,
) -> Result<PoolManager> {
    let pool_manager = Arc::new(DashMap::new());

    // 从active_remotes获取当前活跃的IP列表
    let active_ips = active_remotes.read().await;
    let ip_count = active_ips.len();
    info!("开始初始化连接池，远程数量: {}", ip_count);

    // 为每个远程IP创建连接池
    let pool_creation_tasks: Vec<_> = active_ips
        .iter()
        .map(|ip| {
            let pools = pool_manager.clone();
            let ip = *ip;
            let port = remotes_config.default_remote_port;
            let pool_size = 5; // 默认池大小
            let buffer_size = 10; // 默认缓冲区大小

            tokio::spawn(async move {
                create_pool_for_ip(ip, port, pools, pool_size, buffer_size).await;
            })
        })
        .collect();

    // 等待所有连接池创建完成
    if let Err(e) = try_join_all(pool_creation_tasks).await {
        error!("创建连接池失败: {}", e);
        return Err(anyhow::anyhow!("连接池初始化失败: {}", e));
    }

    info!("连接池初始化完成");
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
