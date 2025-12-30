use crate::config::AppConfig;
use crate::state::IpManager;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

pub async fn start_server(
    config: Arc<AppConfig>,
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
                let config = config.clone();
                let ip_manager = ip_manager.clone();

                tokio::spawn(async move {
                    handle_connection(client_stream, client_addr, config, ip_manager)
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
    config: Arc<AppConfig>,
    ip_manager: IpManager,
) {
    // Get multiple candidate IPs for concurrent connection using new strategy
    let candidate_ips = ip_manager.get_target_ips(
        &config.target_colos,
        config.selection_random_n_subnets,
        config.selection_random_m_ips
    );
    
    if candidate_ips.is_empty() {
        error!("No available IPs for concurrent connection (strategy: top {}% subnets -> {} subnets -> {} IPs)",
            config.selection_top_k_percent * 100.0,
            config.selection_random_n_subnets,
            config.selection_random_m_ips
        );
        return;
    }

    info!("Trying concurrent connections to {} candidates for client {}", candidate_ips.len(), client_addr);

    // Create concurrent connection tasks
    let mut connection_tasks = Vec::new();
    
    for ip in candidate_ips {
        let target_addr = SocketAddr::new(ip, config.target_port);
        let task = tokio::spawn(async move {
            let connect_result = timeout(Duration::from_millis(1500), TcpStream::connect(target_addr)).await;
            
            match connect_result {
                Ok(Ok(stream)) => {
                    // Optimize socket
                    if let Err(e) = stream.set_nodelay(true) {
                        warn!("Failed to set nodelay on connection to {}: {}", ip, e);
                    }
                    
                    // Connection successful
                    Some((stream, ip, std::time::Instant::now()))
                }
                _ => None,
            }
        });
        connection_tasks.push(task);
    }

    // Wait for the first successful connection
    let mut fastest_connection: Option<(TcpStream, std::net::IpAddr, std::time::Instant)> = None;
    
    // Use a short timeout to find the fastest connection
    let timeout_duration = Duration::from_millis(2000);
    let start_time = std::time::Instant::now();
    
    while start_time.elapsed() < timeout_duration && !connection_tasks.is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
        
        let (done, pending): (Vec<_>, Vec<_>) = connection_tasks.into_iter().partition(|t| t.is_finished());
        connection_tasks = pending;
        
        // Check if any task has a successful result
        for task in done {
            if let Ok(Some(connection)) = task.await {
                fastest_connection = Some(connection);
                break;
            }
        }
        
        if fastest_connection.is_some() {
            break;
        }
    }

    // Abort remaining connection tasks
    for task in &connection_tasks {
        task.abort();
    }

    let mut remote_stream = match fastest_connection {
        Some((stream, ip, connect_time)) => {
            let connection_time = connect_time.elapsed();
            info!("Using fastest connection to {} for client {} (established in {:?})", 
                  ip, client_addr, connection_time);
            stream
        }
        None => {
            warn!("All concurrent connections failed for {}, falling back to single connection attempt", client_addr);
            
            // Fallback: try single connection with longer timeout
            // Get just 1 IP (n=1, m=1)
            let fallback_ips = ip_manager.get_target_ips(&config.target_colos, 1, 1);
            
            if fallback_ips.is_empty() {
                 error!("No available IPs for fallback connection");
                 return;
            }
            
            let target_ip = fallback_ips[0];
            let target_addr = SocketAddr::new(target_ip, config.target_port);
            match timeout(Duration::from_secs(5), TcpStream::connect(target_addr)).await {
                Ok(Ok(stream)) => {
                    if let Err(e) = stream.set_nodelay(true) {
                        warn!("Failed to set nodelay on fallback connection: {}", e);
                    }
                    stream
                }
                Ok(Err(e)) => {
                    error!("Failed to establish fallback connection to {}: {}", target_addr, e);
                    return;
                }
                Err(e) => {
                    error!("Timeout connecting to {}: {}", target_addr, e);
                    return;
                }
            }
        }
    };

    // Bidirectional copy
    match copy_bidirectional(&mut client_stream, &mut remote_stream).await {
        Ok((bytes_tx, bytes_rx)) => {
            info!(
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