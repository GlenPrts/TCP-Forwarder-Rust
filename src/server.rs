use crate::config::AppConfig;
use crate::state::IpManager;
use futures::stream::{FuturesUnordered, StreamExt};
use socket2::{SockRef, TcpKeepalive};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::copy_bidirectional_with_sizes;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
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
/// TCP Keepalive 空闲时间（秒）
const KEEPALIVE_TIME_SECS: u64 = 60;
/// TCP Keepalive 探测间隔（秒）
const KEEPALIVE_INTERVAL_SECS: u64 = 10;
/// 最大并发连接数
const MAX_CONCURRENT_CONNECTIONS: usize = 1024;

/// 启动 TCP 转发服务器
///
/// # 参数
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
/// - `cancel_token`: 取消令牌
pub async fn start_server(
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    cancel_token: CancellationToken,
) {
    let listener = match TcpListener::bind(config.bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind to {}: {}", config.bind_addr, e);
            return;
        }
    };

    info!("TCP Forwarder listening on {}", config.bind_addr);

    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                info!("TCP server received shutdown signal");
                break;
            }
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((client_stream, client_addr)) => {
                        process_new_connection(
                            client_stream,
                            client_addr,
                            config.clone(),
                            ip_manager.clone(),
                            semaphore.clone(),
                            cancel_token.clone(),
                        ).await;
                    }
                    Err(e) => {
                        error!("Failed to accept connection: {}", e);
                    }
                }
            }
        }
    }

    info!("TCP server stopped");
}

/// 处理新连接请求
///
/// # 参数
/// - `stream`: 客户端连接流
/// - `addr`: 客户端地址
/// - `config`: 配置
/// - `ip_manager`: IP 管理器
/// - `semaphore`: 并发信号量
/// - `cancel_token`: 取消令牌
async fn process_new_connection(
    stream: TcpStream,
    addr: SocketAddr,
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    semaphore: Arc<Semaphore>,
    cancel_token: CancellationToken,
) {
    debug!("Accepted connection from {}", addr);

    // 获取并发许可，支持取消
    let permit = tokio::select! {
        _ = cancel_token.cancelled() => return,
        p = semaphore.acquire_owned() => match p {
            Ok(p) => p,
            Err(_) => return,
        }
    };

    tokio::spawn(async move {
        // 保持许可直到任务结束
        let _permit = permit;
        if let Err(e) = handle_connection(stream, addr, config, ip_manager).await {
            debug!("Connection handling error for {}: {}", addr, e);
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
) -> anyhow::Result<()> {
    // P0 Bug fix: 配置 Keepalive 和 NODELAY
    let _ = client_stream.set_nodelay(true);
    let _ = configure_keepalive(&client_stream);

    let mut remote_stream = connect_to_remote(
        &config,
        &ip_manager,
        client_addr
    ).await?;

    // 双向数据复制
    match copy_bidirectional_with_sizes(
        &mut client_stream,
        &mut remote_stream,
        BUFFER_SIZE,
        BUFFER_SIZE,
    )
    .await
    {
        Ok((bytes_tx, bytes_rx)) => {
            let peer = remote_stream
                .peer_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| "unknown".to_string());

            info!(
                "Connection closed: {} -> {} (TX: {} bytes, RX: {} bytes)",
                client_addr, peer, bytes_tx, bytes_rx
            );
        }
        Err(e) => {
            debug!("Connection error with {}: {}", client_addr, e);
        }
    }

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
    client_addr: SocketAddr,
) -> anyhow::Result<TcpStream> {
    let candidate_ips = ip_manager.get_target_ips(
        &config.target_colos,
        config.selection_random_n_subnets,
        config.selection_random_m_ips,
    );

    if candidate_ips.is_empty() {
        error!(
            "No available IPs (strategy: top {}% -> {} subnets -> {} IPs)",
            config.selection_top_k_percent * 100.0,
            config.selection_random_n_subnets,
            config.selection_random_m_ips
        );
        return Err(anyhow::anyhow!("No available IPs"));
    }

    info!(
        "Trying concurrent connections to {} candidates for client {}",
        candidate_ips.len(),
        client_addr
    );

    if let Some(result) = race_connections(
        &candidate_ips,
        config.target_port
    ).await {
        info!(
            "Fastest connection: {} for {} ({:?})",
            result.ip, client_addr, result.connect_time
        );
        return Ok(result.stream);
    }

    warn!("All connections failed for {}, falling back", client_addr);

    // 回退：使用更长的超时时间尝试单个连接
    let fallback_ips = ip_manager.get_target_ips(&config.target_colos, 1, 1);

    if fallback_ips.is_empty() {
        error!("No available IPs for fallback connection");
        return Err(anyhow::anyhow!("No available IPs for fallback"));
    }

    connect_with_fallback(fallback_ips[0], config.target_port).await
}

/// 尝试单个连接
///
/// # 参数
/// - `ip`: 目标 IP
/// - `port`: 目标端口
/// - `cancel_token`: 取消令牌
///
/// # 返回值
/// - `Option<ConnectionResult>`: 连接结果
async fn try_connect_single(
    ip: IpAddr,
    port: u16,
    cancel_token: CancellationToken,
) -> Option<ConnectionResult> {
    let target_addr = SocketAddr::new(ip, port);

    tokio::select! {
        _ = cancel_token.cancelled() => {
            debug!("Connection task cancelled for {}", ip);
            None
        }
        result = async {
            let start = Instant::now();
            let connect_result = timeout(
                Duration::from_millis(CONNECT_TIMEOUT_MS),
                TcpStream::connect(target_addr),
            )
            .await;

            match connect_result {
                Ok(Ok(stream)) => {
                    let _ = stream.set_nodelay(true);
                    let _ = configure_keepalive(&stream);
                    Some(ConnectionResult {
                        stream,
                        ip,
                        connect_time: start.elapsed(),
                    })
                }
                _ => None,
            }
        } => result,
    }
}

/// 等待第一个成功连接
///
/// # 参数
/// - `tasks`: 异步任务集合
/// - `cancel_token`: 取消令牌
///
/// # 返回值
/// - `Option<ConnectionResult>`: 第一个成功的连接结果
async fn await_first_success(
    mut tasks: FuturesUnordered<
        impl std::future::Future<Output = Option<ConnectionResult>>
    >,
    cancel_token: CancellationToken,
) -> Option<ConnectionResult> {
    tokio::select! {
        result = async {
            while let Some(res) = tasks.next().await {
                if let Some(conn) = res {
                    cancel_token.cancel();
                    return Some(conn);
                }
            }
            None
        } => result,
        _ = tokio::time::sleep(Duration::from_millis(SELECTION_TIMEOUT_MS)) => {
            debug!("Connection selection timed out");
            cancel_token.cancel();
            None
        }
    }
}

/// 并发连接竞速 - 返回最快建立的连接
///
/// # 参数
/// - `candidate_ips`: 候选 IP 列表
/// - `target_port`: 目标端口
///
/// # 返回值
/// - `Option<ConnectionResult>`: 最快建立的连接
async fn race_connections(
    candidate_ips: &[IpAddr],
    target_port: u16
) -> Option<ConnectionResult> {
    if candidate_ips.is_empty() {
        return None;
    }

    let cancel_token = CancellationToken::new();
    let tasks = FuturesUnordered::new();

    for &ip in candidate_ips {
        tasks.push(try_connect_single(
            ip,
            target_port,
            cancel_token.clone(),
        ));
    }

    await_first_success(tasks, cancel_token).await
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
    port: u16
) -> anyhow::Result<TcpStream> {
    let target_addr = SocketAddr::new(ip, port);

    match timeout(
        Duration::from_secs(FALLBACK_TIMEOUT_SECS),
        TcpStream::connect(target_addr),
    )
    .await
    {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            let _ = configure_keepalive(&stream);
            Ok(stream)
        }
        Ok(Err(e)) => {
            error!(
                "Failed to establish fallback connection to {}: {}",
                target_addr, e
            );
            Err(e.into())
        }
        Err(_) => {
            error!("Timeout connecting to {}", target_addr);
            Err(anyhow::anyhow!("Connection timeout"))
        }
    }
}

/// 配置 TCP Keepalive
///
/// # 参数
/// - `stream`: TCP 连接流
///
/// # 返回值
/// - `std::io::Result<()>`: 配置结果
fn configure_keepalive(stream: &TcpStream) -> std::io::Result<()> {
    let socket_ref = SockRef::from(stream);
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(KEEPALIVE_TIME_SECS))
        .with_interval(Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
    socket_ref.set_tcp_keepalive(&keepalive)
}
