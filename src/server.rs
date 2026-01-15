use crate::config::AppConfig;
use crate::state::IpManager;
use futures::stream::{FuturesUnordered, StreamExt};
use socket2::{SockRef, TcpKeepalive};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::copy_bidirectional;
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

/// 启动 TCP 转发服务器
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

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                info!("TCP server received shutdown signal");
                break;
            }
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((client_stream, client_addr)) => {
                        debug!("Accepted connection from {}", client_addr);
                        let config = config.clone();
                        let ip_manager = ip_manager.clone();

                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(client_stream, client_addr, config, ip_manager).await {
                                debug!("Connection handling error for {}: {}", client_addr, e);
                            }
                        });
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

/// 连接结果
struct ConnectionResult {
    stream: TcpStream,
    ip: IpAddr,
    connect_time: Duration,
}

fn set_tcp_keepalive(stream: &TcpStream) -> anyhow::Result<()> {
    let socket_ref = SockRef::from(stream);
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(10));
    socket_ref.set_tcp_keepalive(&keepalive)?;
    Ok(())
}

/// 处理单个客户端连接
async fn handle_connection(
    mut client_stream: TcpStream,
    client_addr: SocketAddr,
    config: Arc<AppConfig>,
    ip_manager: IpManager,
) -> anyhow::Result<()> {
    if let Err(e) = set_tcp_keepalive(&client_stream) {
        warn!("Failed to set keepalive for client {}: {}", client_addr, e);
    }

    // 获取候选 IP 列表
    let candidate_ips = ip_manager.get_target_ips(
        &config.target_colos,
        config.selection_random_n_subnets,
        config.selection_random_m_ips,
    );

    if candidate_ips.is_empty() {
        error!(
            "No available IPs for concurrent connection (strategy: top {}% subnets -> {} subnets -> {} IPs)",
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

    // 尝试并发连接
    let mut remote_stream = match race_connections(&candidate_ips, config.target_port).await {
        Some(result) => {
            info!(
                "Using fastest connection to {} for client {} (established in {:?})",
                result.ip, client_addr, result.connect_time
            );
            result.stream
        }
        None => {
            warn!(
                "All concurrent connections failed for {}, falling back to single connection attempt",
                client_addr
            );

            // 回退：使用更长的超时时间尝试单个连接
            let fallback_ips = ip_manager.get_target_ips(&config.target_colos, 1, 1);

            if fallback_ips.is_empty() {
                error!("No available IPs for fallback connection");
                return Err(anyhow::anyhow!("No available IPs for fallback"));
            }

            connect_with_fallback(fallback_ips[0], config.target_port).await?
        }
    };

    // 双向数据复制
    match copy_bidirectional(&mut client_stream, &mut remote_stream).await {
        Ok((bytes_tx, bytes_rx)) => {
            info!(
                "Connection closed: {} -> {} (TX: {} bytes, RX: {} bytes)",
                client_addr,
                remote_stream
                    .peer_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|_| "unknown".to_string()),
                bytes_tx,
                bytes_rx
            );
        }
        Err(e) => {
            debug!("Connection error with {}: {}", client_addr, e);
        }
    }

    Ok(())
}

/// 并发连接竞速 - 返回最快建立的连接
async fn race_connections(candidate_ips: &[IpAddr], target_port: u16) -> Option<ConnectionResult> {
    if candidate_ips.is_empty() {
        return None;
    }

    // 使用 CancellationToken 来取消未完成的任务
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let mut tasks = FuturesUnordered::new();

    for &ip in candidate_ips {
        let target_addr = SocketAddr::new(ip, target_port);
        let cancel_clone = cancel_token.clone();
        
        tasks.push(async move {
            tokio::select! {
                _ = cancel_clone.cancelled() => {
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
                            // 优化 socket
                            let _ = stream.set_nodelay(true);
                            if let Err(e) = set_tcp_keepalive(&stream) {
                                warn!("Failed to set keepalive for target {}: {}", ip, e);
                            }
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
        });
    }

    // 等待第一个成功的连接或超时
    let result = tokio::select! {
        result = async {
            while let Some(res) = tasks.next().await {
                if let Some(conn) = res {
                    // 找到最快连接，取消其他任务
                    cancel_token.cancel();
                    return Some(conn);
                }
            }
            None
        } => result,
        _ = tokio::time::sleep(Duration::from_millis(SELECTION_TIMEOUT_MS)) => {
            debug!("Connection selection timed out");
            // 超时后取消所有剩余任务
            cancel_token.cancel();
            None
        }
    };

    result
}

/// 使用回退策略连接
async fn connect_with_fallback(ip: IpAddr, port: u16) -> anyhow::Result<TcpStream> {
    let target_addr = SocketAddr::new(ip, port);

    match timeout(
        Duration::from_secs(FALLBACK_TIMEOUT_SECS),
        TcpStream::connect(target_addr),
    )
    .await
    {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            if let Err(e) = set_tcp_keepalive(&stream) {
                warn!(
                    "Failed to set keepalive for fallback {}: {}",
                    target_addr, e
                );
            }
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
