use crate::config::AppConfig;
use crate::metrics::ForwardMetrics;
use crate::pool::ConnectionPool;
use crate::state::IpManager;
use futures::stream::{FuturesUnordered, StreamExt};
use socket2::{SockRef, TcpKeepalive};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::copy_bidirectional_with_sizes;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// 连接超时时间（毫秒）
const CONNECT_TIMEOUT_MS: u64 = 1500;
/// 总体连接选择超时时间（毫秒）
const SELECTION_TIMEOUT_MS: u64 = 2000;
/// 回退连接超时时间（秒）
const FALLBACK_TIMEOUT_SECS: u64 = 5;
/// 缓冲区大小 64KB
const BUFFER_SIZE: usize = 65536;

/// 启动 TCP 转发服务器
///
/// # 参数
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
/// - `cancel_token`: 取消令牌
pub async fn start_server(
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    pool: Option<Arc<ConnectionPool>>,
    cancel_token: CancellationToken,
    metrics: Arc<ForwardMetrics>,
) {
    let listener = match TcpListener::bind(config.bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind to {}: {}", config.bind_addr, e);
            return;
        }
    };

    info!("TCP Forwarder listening on {}", config.bind_addr);

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                info!("TCP server received shutdown signal");
                break;
            }
            accept_result = listener.accept() => {
                handle_accept_result(
                    accept_result,
                    &config,
                    &ip_manager,
                    &pool,
                    &cancel_token,
                    &metrics,
                );
            }
        }
    }

    info!("TCP server stopped");
}

/// 处理监听器接收到的连接结果
///
/// 立即 spawn 异步任务，不阻塞 accept 循环。
/// FD permit 获取在 spawn 内部完成。
fn handle_accept_result(
    accept_result: std::io::Result<(TcpStream, SocketAddr)>,
    config: &Arc<AppConfig>,
    ip_manager: &IpManager,
    pool: &Option<Arc<ConnectionPool>>,
    cancel_token: &CancellationToken,
    metrics: &Arc<ForwardMetrics>,
) {
    let (client_stream, client_addr) = match accept_result {
        Ok(res) => res,
        Err(e) => {
            error!("Failed to accept connection: {}", e);
            return;
        }
    };

    debug!("Accepted connection from {}", client_addr);

    let config = config.clone();
    let ip_manager = ip_manager.clone();
    let pool = pool.clone();
    let cancel_token = cancel_token.clone();
    let metrics = metrics.clone();

    metrics.inc_total_connections();
    metrics.inc_active();

    tokio::spawn(async move {
        // 在 spawn 内部获取许可，避免阻塞 accept 循环
        let permit = tokio::select! {
            _ = cancel_token.cancelled() => {
                metrics.dec_active();
                return;
            }
            p = ip_manager.acquire_fd_permit() => match p {
                Ok(p) => p,
                Err(_) => {
                    metrics.dec_active();
                    return;
                }
            }
        };
        let _permit = permit;
        let result = handle_connection(
            client_stream,
            client_addr,
            config,
            ip_manager,
            pool,
            &metrics,
        )
        .await;
        metrics.dec_active();
        if let Err(e) = result {
            debug!(
                "Connection handling error for {}: {}",
                client_addr, e
            );
        }
    });
}

/// 连接结果
struct ConnectionResult {
    stream: TcpStream,
    ip: IpAddr,
    connect_time: Duration,
}

/// 处理单个客户端连接
///
/// # 参数
/// - `client_stream`: 客户端 TCP 连接流
/// - `client_addr`: 客户端地址
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
///
/// # 返回值
/// - `anyhow::Result<()>`: 处理结果
async fn handle_connection(
    mut client_stream: TcpStream,
    client_addr: SocketAddr,
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    pool: Option<Arc<ConnectionPool>>,
    metrics: &ForwardMetrics,
) -> anyhow::Result<()> {
    let _ = client_stream.set_nodelay(true);
    let _ = configure_keepalive(&client_stream, &config);

    let mut remote_stream = connect_to_remote(
        &config, &ip_manager, &pool, client_addr, metrics,
    )
    .await?;
    // 连接关闭后 peer_addr() 可能失败，提前保存
    let peer = remote_stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    // 双向数据复制
    let copy_result = copy_bidirectional_with_sizes(
        &mut client_stream,
        &mut remote_stream,
        BUFFER_SIZE,
        BUFFER_SIZE,
    )
    .await;

    let (bytes_tx, bytes_rx) = match copy_result {
        Ok(res) => res,
        Err(e) => {
            debug!("Connection error with {}: {}", client_addr, e);
            return Ok(());
        }
    };

    metrics.add_bytes(bytes_tx, bytes_rx);

    info!(
        "Connection closed: {} -> {} (TX: {} bytes, RX: {} bytes)",
        client_addr, peer, bytes_tx, bytes_rx
    );

    Ok(())
}

/// 建立与远程目标的连接（包含竞速和回退机制）
///
/// # 参数
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
/// - `client_addr`: 客户端地址
///
/// # 返回值
/// - `anyhow::Result<TcpStream>`: 建立的 TCP 连接
async fn connect_to_remote(
    config: &Arc<AppConfig>,
    ip_manager: &IpManager,
    pool: &Option<Arc<ConnectionPool>>,
    client_addr: SocketAddr,
    metrics: &ForwardMetrics,
) -> anyhow::Result<TcpStream> {
    if let Some(ref p) = pool {
        if let Some((stream, ip)) = p.acquire() {
            info!(
                "Using pooled connection to {} for {}",
                ip, client_addr
            );
            metrics.inc_pool_hit();
            return Ok(stream);
        }
    }

    let stream = connect_via_race(
        config, ip_manager, client_addr, metrics,
    )
    .await?;

    // 延迟配置：仅对竞速胜出者（非池化连接）进行配置
    let _ = stream.set_nodelay(true);
    let _ = configure_keepalive(&stream, config);

    Ok(stream)
}

/// 通过竞速建立远程连接（原始路径）
///
/// # 参数
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
/// - `client_addr`: 客户端地址
///
/// # 返回值
/// - `anyhow::Result<TcpStream>`: 建立的 TCP 连接
async fn connect_via_race(
    config: &Arc<AppConfig>,
    ip_manager: &IpManager,
    client_addr: SocketAddr,
    metrics: &ForwardMetrics,
) -> anyhow::Result<TcpStream> {
    let candidate_ips = ip_manager.get_target_ips(
        &config.target_colos,
        config.selection_random_n_subnets,
        config.selection_random_m_ips,
    );

    if candidate_ips.is_empty() {
        return Err(handle_no_ips(config));
    }

    info!(
        "Trying concurrent connections to {} candidates for client {}",
        candidate_ips.len(),
        client_addr
    );

    if let Some(result) = race_connections(
        &candidate_ips,
        config.target_port,
        config.staggered_delay_ms,
        ip_manager,
    )
    .await
    {
        info!(
            "Fastest connection: {} for {} ({:?})",
            result.ip, client_addr, result.connect_time
        );
        metrics.inc_race_success();
        return Ok(result.stream);
    }

    metrics.inc_race_failure();
    warn!(
        "All connections failed for {}, falling back",
        client_addr
    );
    perform_fallback(config, ip_manager).await
}

/// 处理无可用 IP 的情况
fn handle_no_ips(config: &AppConfig) -> anyhow::Error {
    error!(
        "No available IPs (strategy: top {}% -> {} subnets -> {} IPs)",
        config.selection_top_k_percent * 100.0,
        config.selection_random_n_subnets,
        config.selection_random_m_ips
    );
    anyhow::anyhow!("No available IPs")
}

/// 执行回退连接
async fn perform_fallback(config: &AppConfig, ip_manager: &IpManager) -> anyhow::Result<TcpStream> {
    let fallback_ips = ip_manager.get_target_ips(&config.target_colos, 1, 1);

    if fallback_ips.is_empty() {
        error!("No available IPs for fallback connection");
        return Err(anyhow::anyhow!("No available IPs for fallback"));
    }

    connect_with_fallback(fallback_ips[0], config.target_port, ip_manager).await
}

/// 尝试单个连接
///
/// # 参数
/// - `ip`: 目标 IP
/// - `port`: 目标端口
/// - `ip_manager`: IP 管理器
/// - `cancel_token`: 取消令牌
///
/// # 返回值
/// - `Option<ConnectionResult>`: 连接结果
async fn try_connect_single(
    ip: IpAddr,
    port: u16,
    ip_manager: &IpManager,
    cancel_token: CancellationToken,
) -> Option<ConnectionResult> {
    let target_addr = SocketAddr::new(ip, port);

    tokio::select! {
        _ = cancel_token.cancelled() => {
            debug!("Connection task cancelled for {}", ip);
            None
        }
        result = async {
            // 获取许可，如果失败则直接返回 None
            let permit = match ip_manager.acquire_fd_permit().await {
                Ok(p) => p,
                Err(_) => return None,
            };

            let start = Instant::now();
            let connect_result = timeout(
                Duration::from_millis(CONNECT_TIMEOUT_MS),
                TcpStream::connect(target_addr),
            )
            .await;

            let stream = match connect_result {
                Ok(Ok(s)) => s,
                _ => {
                    drop(permit);
                    return None;
                }
            };

            Some(ConnectionResult {
                stream,
                ip,
                connect_time: start.elapsed(),
            })
        } => result,
    }
}

/// 并发连接竞速 - 返回最快建立的连接（阶梯启动）
///
/// # 参数
/// - `candidate_ips`: 候选 IP 列表
/// - `target_port`: 目标端口
/// - `staggered_delay_ms`: 阶梯启动延迟
/// - `ip_manager`: IP 管理器
///
/// # 返回值
/// - `Option<ConnectionResult>`: 最快建立的连接
async fn race_connections(
    candidate_ips: &[IpAddr],
    target_port: u16,
    staggered_delay_ms: u64,
    ip_manager: &IpManager,
) -> Option<ConnectionResult> {
    if candidate_ips.is_empty() {
        return None;
    }
    let cancel_token = CancellationToken::new();
    let mut tasks = FuturesUnordered::new();
    let mut next_ip_idx = 0;
    let selection_timeout = Duration::from_millis(SELECTION_TIMEOUT_MS);
    let total_timeout_fut = tokio::time::sleep(selection_timeout);
    tokio::pin!(total_timeout_fut);
    while next_ip_idx < candidate_ips.len() || !tasks.is_empty() {
        if next_ip_idx < candidate_ips.len() {
            let ip = candidate_ips[next_ip_idx];
            debug!(
                "Starting staggered connection attempt {}/{} to {}",
                next_ip_idx + 1,
                candidate_ips.len(),
                ip
            );
            tasks.push(try_connect_single(
                ip,
                target_port,
                ip_manager,
                cancel_token.clone(),
            ));
            next_ip_idx += 1;
        }
        let mut delay = Duration::from_secs(86400);
        if next_ip_idx < candidate_ips.len() {
            delay = Duration::from_millis(staggered_delay_ms);
        }
        let interval_fut = tokio::time::sleep(delay);
        tokio::pin!(interval_fut);
        tokio::select! {
            _ = &mut total_timeout_fut => {
                debug!("Connection selection timed out");
                cancel_token.cancel();
                return None;
            }
            res = tasks.next(), if !tasks.is_empty() => {
                if let Some(Some(conn)) = res {
                    cancel_token.cancel();
                    return Some(conn);
                }
            }
            _ = &mut interval_fut, if next_ip_idx < candidate_ips.len() => {}
        }
    }
    None
}

/// 使用回退策略连接
///
/// # 参数
/// - `ip`: 目标 IP
/// - `port`: 目标端口
///
/// # 返回值
/// - `anyhow::Result<TcpStream>`: 建立的 TCP 连接
async fn connect_with_fallback(
    ip: IpAddr,
    port: u16,
    ip_manager: &IpManager,
) -> anyhow::Result<TcpStream> {
    let _permit = ip_manager.acquire_fd_permit().await.ok();
    let target_addr = SocketAddr::new(ip, port);

    let connect_result = timeout(
        Duration::from_secs(FALLBACK_TIMEOUT_SECS),
        TcpStream::connect(target_addr),
    )
    .await;

    let stream = match connect_result {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            error!(
                "Failed to establish fallback connection to {}: {}",
                target_addr, e
            );
            return Err(e.into());
        }
        Err(_) => {
            error!("Timeout connecting to {}", target_addr);
            return Err(anyhow::anyhow!("Connection timeout"));
        }
    };

    Ok(stream)
}

/// 配置 TCP Keepalive
///
/// # 参数
/// - `stream`: TCP 连接流
/// - `config`: 应用配置
///
/// # 返回值
/// - `std::io::Result<()>`: 配置结果
fn configure_keepalive(stream: &TcpStream, config: &AppConfig) -> std::io::Result<()> {
    let socket_ref = SockRef::from(stream);
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(config.tcp_keepalive.time_secs))
        .with_interval(Duration::from_secs(config.tcp_keepalive.interval_secs));
    socket_ref.set_tcp_keepalive(&keepalive)
}
