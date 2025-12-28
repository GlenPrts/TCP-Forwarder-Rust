use crate::config::AppConfig;
use crate::state::IpManager;
use socket2::{SockRef, TcpKeepalive};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::mpsc::Sender;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

#[derive(Debug)]
pub struct WarmConnection {
    pub stream: TcpStream,
    pub peer_ip: IpAddr,
}

pub async fn maintain_pool(
    manager: IpManager,
    tx: Sender<WarmConnection>,
    config: Arc<AppConfig>,
) {
    info!("Starting warm connection pool maintainer...");
    
    // Dynamic pool sizing logic
    // We can't easily resize the channel itself, but we can control how fast we fill it
    // or we can use a semaphore to limit active connections if we were managing them differently.
    // Since we use a bounded channel, the channel size IS the pool size.
    // To dynamically adjust, we would need to recreate the channel or use a larger channel and limit the fill depth.
    // Recreating channel is hard because receiver is in another task.
    // So we will use a "target fill level" logic.
    // However, `tx.capacity()` returns remaining capacity. `tx.max_capacity()` is fixed.
    // Let's assume we want to keep the pool full for now as per original design, 
    // but maybe slow down filling if we detect low usage?
    // Without usage stats fed back to this task, it's hard to be truly dynamic based on traffic.
    // For now, let's stick to the robust filling logic with Keepalive.
    // If we really want dynamic sizing, we'd need a shared atomic counter of "active user connections" 
    // and adjust the target pool size based on that.
    // Let's implement a simple backoff if we are filling too fast (churning).
    
    let mut last_fill_time = Instant::now();
    let mut fill_count = 0;

    loop {
        // Check if we need more connections
        if tx.capacity() == 0 {
            sleep(Duration::from_millis(100)).await;
            continue;
        }

        // Rate limit filling to avoid hammering if pool is drained instantly (e.g. attack or bug)
        fill_count += 1;
        if fill_count > 10 {
            if last_fill_time.elapsed() < Duration::from_secs(1) {
                sleep(Duration::from_millis(100)).await;
            }
            last_fill_time = Instant::now();
            fill_count = 0;
        }

        // Get best IP
        let target_ip = match manager.get_best_ip(&config.target_colos) {
            Some(ip) => ip,
            None => {
                warn!("No valid IPs found in pool, waiting for scanner...");
                sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        // Connect
        let target_addr = SocketAddr::new(target_ip, config.target_port);
        debug!("Establishing warm connection to {}", target_addr);

        match tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(target_addr)).await {
            Ok(Ok(stream)) => {
                // Optimize socket
                if let Err(e) = stream.set_nodelay(true) {
                    error!("Failed to set nodelay on {}: {}", target_ip, e);
                    continue;
                }

                // Set TCP Keepalive
                let sock_ref = SockRef::from(&stream);
                let keepalive = TcpKeepalive::new()
                    .with_time(Duration::from_secs(60))
                    .with_interval(Duration::from_secs(10));
                
                // Retries might not be available on all platforms or require specific features.
                // Removing explicit retries configuration to ensure compilation.
                
                if let Err(e) = sock_ref.set_tcp_keepalive(&keepalive) {
                     warn!("Failed to set keepalive on {}: {}", target_ip, e);
                }

                let conn = WarmConnection {
                    stream,
                    peer_ip: target_ip,
                };

                // Send to pool
                if let Err(_) = tx.send(conn).await {
                    error!("Pool receiver dropped, stopping maintainer task.");
                    break;
                }
                debug!("Added warm connection to pool: {}", target_ip);
            }
            Ok(Err(e)) => {
                warn!("Failed to connect to {}: {}", target_ip, e);
                sleep(Duration::from_millis(500)).await;
            }
            Err(_) => {
                warn!("Connection timeout to {}", target_ip);
            }
        }
    }
}