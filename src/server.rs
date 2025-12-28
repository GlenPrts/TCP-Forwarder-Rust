use crate::config::AppConfig;
use crate::pool::WarmConnection;
use crate::state::IpManager;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::Receiver;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

pub async fn start_server(
    config: Arc<AppConfig>,
    pool_rx: Arc<Mutex<Receiver<WarmConnection>>>,
    ip_manager: IpManager,
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
        match listener.accept().await {
            Ok((client_stream, client_addr)) => {
                debug!("Accepted connection from {}", client_addr);
                let pool_rx = pool_rx.clone();
                let config = config.clone();
                let ip_manager = ip_manager.clone();

                tokio::spawn(async move {
                    handle_connection(client_stream, client_addr, pool_rx, config, ip_manager)
                        .await;
                });
            }
            Err(e) => {
                error!("Failed to accept connection: {}", e);
            }
        }
    }
}

async fn handle_connection(
    mut client_stream: TcpStream,
    client_addr: SocketAddr,
    pool_rx: Arc<Mutex<Receiver<WarmConnection>>>,
    config: Arc<AppConfig>,
    ip_manager: IpManager,
) {
    // Try to get a warm connection
    let mut warm_conn = None;
    
    // Loop to find a valid warm connection
    loop {
        let maybe_conn = {
            let mut rx = pool_rx.lock().await;
            rx.try_recv().ok()
        };

        match maybe_conn {
            Some(conn) => {
                // Check if connection is still alive using peek
                let mut buf = [0u8; 1];
                match conn.stream.peek(&mut buf).await {
                    Ok(0) => {
                        // Connection closed by peer
                        debug!("Warm connection to {} is dead (peek 0), discarding", conn.peer_ip);
                        continue;
                    }
                    Ok(_) => {
                        // Connection seems alive (has data or just open)
                        // Note: peek returning > 0 means there is data, which is unexpected for a fresh connection 
                        // unless server sent banner. But for Cloudflare CDN, usually client speaks first.
                        // If peek blocks, it means no data yet, but we are using async peek which might return immediately if data is there?
                        // Wait, TcpStream::peek is async. If no data is available, it waits?
                        // Actually, we want non-blocking check or just assume it's alive if no error.
                        // But standard peek waits for data.
                        // A better check for "is closed" without blocking is tricky in pure async Rust without polling.
                        // However, for a warm pool, we just established it. 
                        // If it was closed, peek might return 0 immediately.
                        // If it's open and idle, peek will block. We don't want to block.
                        // So we can use `try_read` or similar? No, `try_read` reads data.
                        // Let's skip complex peek for now and rely on the fact that we just created it recently
                        // and we have Keepalive.
                        // BUT, the user asked for Peek detection.
                        // To do non-blocking peek: set non-blocking, peek, handle WouldBlock.
                        // But tokio stream is already non-blocking.
                        // If we call `peek`, it awaits data. We can wrap it in a timeout(0).
                        
                        match tokio::time::timeout(std::time::Duration::from_micros(1), conn.stream.peek(&mut buf)).await {
                            Ok(Ok(0)) => {
                                debug!("Warm connection to {} is dead (peek 0), discarding", conn.peer_ip);
                                continue;
                            }
                            Ok(Err(_)) => {
                                // Error peeking, discard
                                debug!("Warm connection to {} error peeking, discarding", conn.peer_ip);
                                continue;
                            }
                            _ => {
                                // Timeout (no data yet) or Data present -> Assume alive
                                warm_conn = Some(conn);
                                break;
                            }
                        }
                    }
                    Err(_) => {
                         debug!("Warm connection to {} error peeking, discarding", conn.peer_ip);
                         continue;
                    }
                }
            }
            None => break, // Pool empty
        }
    }

    let mut remote_stream = match warm_conn {
        Some(conn) => {
            debug!(
                "Using warm connection to {} for client {}",
                conn.peer_ip, client_addr
            );
            conn.stream
        }
        None => {
            warn!("Warm pool empty or all dead, falling back to direct connection for {}", client_addr);
            // Fallback: Connect directly
            let target_ip = match ip_manager.get_best_ip(&config.target_colos) {
                Some(ip) => ip,
                None => {
                    error!("No available IPs for fallback connection");
                    return;
                }
            };
            let target_addr = SocketAddr::new(target_ip, config.target_port);
            match TcpStream::connect(target_addr).await {
                Ok(stream) => {
                    if let Err(e) = stream.set_nodelay(true) {
                        warn!("Failed to set nodelay on fallback connection: {}", e);
                    }
                    stream
                }
                Err(e) => {
                    error!("Failed to establish fallback connection to {}: {}", target_addr, e);
                    return;
                }
            }
        }
    };

    // Bidirectional copy
    match copy_bidirectional(&mut client_stream, &mut remote_stream).await {
        Ok((bytes_tx, bytes_rx)) => {
            debug!(
                "Connection closed: {} -> {} (TX: {} bytes, RX: {} bytes)",
                client_addr,
                remote_stream.peer_addr().unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap()),
                bytes_tx,
                bytes_rx
            );
        }
        Err(e) => {
            debug!("Connection error with {}: {}", client_addr, e);
        }
    }
}