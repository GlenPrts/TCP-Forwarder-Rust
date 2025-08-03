use anyhow::Result;
use dashmap::DashMap;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn, instrument};
use crate::config::RemotesConfig;
use crate::scorer::ScoreBoard;
use crate::selector::ActiveRemotes;

/// 连接池管理器类型定义
pub type PoolManager = Arc<DashMap<IpAddr, PoolState>>;

/// 单个IP的连接池状态
#[derive(Debug)]
pub struct PoolState {
    /// 连接池的发送端，filler任务用它向池中添加连接
    pub sender: Sender<TcpStream>,
    /// 连接池的接收端，用于获取预建立的连接
    pub receiver: Arc<tokio::sync::Mutex<Receiver<TcpStream>>>,
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

/// 创建连接池管理器
pub fn create_pool_manager() -> PoolManager {
    Arc::new(DashMap::new())
}

/// 连接池管理任务
/// 
/// 这个任务监听活跃IP列表的变化，并相应地管理连接池：
/// - 为新增的IP创建连接池和填充任务
/// - 为移除的IP停止填充任务并排空连接池
#[instrument(skip(pool_manager, active_remotes, score_board, remotes_config, pools_config))]
pub async fn pool_manager_task(
    pool_manager: PoolManager,
    active_remotes: ActiveRemotes,
    score_board: ScoreBoard,
    remotes_config: RemotesConfig,
    pools_config: crate::config::PoolsConfig,
) -> Result<()> {
    info!("连接池管理任务已启动");
    
    let mut last_active_ips: Vec<IpAddr> = Vec::new();
    let check_interval = Duration::from_secs(1); // 检查间隔1秒
    
    loop {
        // 获取当前活跃IP列表
        let current_active_ips = {
            let ips = active_remotes.read().await;
            ips.clone()
        };
        
        // 如果IP列表发生变化，更新连接池
        if current_active_ips != last_active_ips {
            debug!("检测到活跃IP列表变化，更新连接池");
            
            // 处理新增的IP
            for ip in &current_active_ips {
                if !last_active_ips.contains(ip) {
                    info!("为新活跃IP {} 创建连接池", ip);
                    create_pool_for_ip(&pool_manager, *ip, &score_board, &remotes_config, &pools_config).await?;
                }
            }
            
            // 处理移除的IP
            for ip in &last_active_ips {
                if !current_active_ips.contains(ip) {
                    info!("移除非活跃IP {} 的连接池", ip);
                    remove_pool_for_ip(&pool_manager, *ip).await;
                }
            }
            
            last_active_ips = current_active_ips;
        }
        
        sleep(check_interval).await;
    }
}

/// 为指定IP创建连接池
async fn create_pool_for_ip(
    pool_manager: &PoolManager,
    ip: IpAddr,
    score_board: &ScoreBoard,
    remotes_config: &RemotesConfig,
    pools_config: &crate::config::PoolsConfig,
) -> Result<()> {
    // 根据策略类型确定连接池大小
    let pool_size = match pools_config.strategy.strategy_type.as_str() {
        "static" => {
            let static_config = pools_config.strategy.static_.as_ref()
                .ok_or_else(|| anyhow::anyhow!("静态策略配置缺失"))?;
            static_config.size_per_remote
        }
        "dynamic" => {
            let dynamic_config = pools_config.strategy.dynamic.as_ref()
                .ok_or_else(|| anyhow::anyhow!("动态策略配置缺失"))?;
            // 暂时使用最小大小，实际动态调整逻辑可以后续扩展
            dynamic_config.min_size
        }
        _ => {
            return Err(anyhow::anyhow!("不支持的连接池策略类型: {}", pools_config.strategy.strategy_type));
        }
    };
    
    // 创建连接池状态
    let pool_state = PoolState::new(pool_size);
    
    // 将连接池添加到管理器
    pool_manager.insert(ip, pool_state);
    
    // 启动填充任务
    if let Some(pool_state_ref) = pool_manager.get(&ip) {
        let pool_state = PoolState {
            sender: pool_state_ref.sender.clone(),
            receiver: pool_state_ref.receiver.clone(),
            is_active: pool_state_ref.is_active.clone(),
        };
        drop(pool_state_ref); // 释放引用
        
        let score_board_clone = score_board.clone();
        let remote_port = remotes_config.default_remote_port;
        let pool_config = pools_config.clone();
        
        tokio::spawn(async move {
            if let Err(e) = filler_task(ip, remote_port, pool_state, score_board_clone, pool_config).await {
                error!("IP {} 的连接池填充任务出错: {}", ip, e);
            }
        });
    }
    
    debug!("已为IP {} 创建连接池和填充任务，池大小: {}", ip, pool_size);
    Ok(())
}

/// 移除指定IP的连接池
async fn remove_pool_for_ip(pool_manager: &PoolManager, ip: IpAddr) {
    if let Some((_, pool_state)) = pool_manager.remove(&ip) {
        // 标记连接池为非活跃状态，这将导致填充任务退出
        pool_state.mark_inactive().await;
        
        // 排空连接池中的现有连接
        let mut receiver = pool_state.receiver.lock().await;
        let mut drained_count = 0;
        
        while let Ok(_stream) = receiver.try_recv() {
            // 连接会在离开作用域时自动关闭
            drained_count += 1;
        }
        
        if drained_count > 0 {
            debug!("已排空IP {} 连接池中的 {} 个连接", ip, drained_count);
        }
    }
}

/// 连接池填充任务
/// 
/// 持续维护指定IP的连接池，确保池中始终有足够的可用连接
#[instrument(skip(pool_state, score_board, config))]
async fn filler_task(
    ip: IpAddr,
    port: u16,
    pool_state: PoolState,
    score_board: ScoreBoard,
    config: crate::config::PoolsConfig,
) -> Result<()> {
    let target_addr = format!("{}:{}", ip, port);
    let connection_timeout = config.common.dial_timeout;
    
    // 根据策略类型确定池大小和填充间隔
    let (pool_size, fill_interval) = match config.strategy.strategy_type.as_str() {
        "static" => {
            let static_config = config.strategy.static_.as_ref()
                .ok_or_else(|| anyhow::anyhow!("静态策略配置缺失"))?;
            (static_config.size_per_remote, Duration::from_millis(100))
        }
        "dynamic" => {
            let dynamic_config = config.strategy.dynamic.as_ref()
                .ok_or_else(|| anyhow::anyhow!("动态策略配置缺失"))?;
            // 暂时使用最小大小，实际动态调整逻辑可以后续扩展
            (dynamic_config.min_size, dynamic_config.scaling.interval)
        }
        _ => {
            return Err(anyhow::anyhow!("不支持的连接池策略类型: {}", config.strategy.strategy_type));
        }
    };
    
    debug!("开始为 {} 填充连接池，目标大小: {}", target_addr, pool_size);
    
    loop {
        // 检查连接池是否仍然活跃
        if !pool_state.is_active().await {
            info!("IP {} 的连接池已标记为非活跃，停止填充任务", ip);
            break;
        }
        
        // 检查连接池发送端是否已关闭
        if pool_state.sender.is_closed() {
            debug!("连接池发送端已关闭: {}", target_addr);
            break;
        }
        
        // 检查连接池是否已满
        // 尝试预留一个发送位置，如果失败说明池已满
        match pool_state.sender.try_reserve() {
            Ok(_permit) => {
                // 有可用空间，继续创建连接
                // 注意：这里我们只是检查容量，不实际发送，所以丢弃permit
            }
            Err(_) => {
                // 连接池已满，暂停一段时间后再检查
                debug!("连接池已满，等待空间释放: {}", target_addr);
                sleep(fill_interval).await;
                continue;
            }
        }
        
        // 尝试创建新连接
        let start_time = std::time::Instant::now();
        
        match tokio::time::timeout(connection_timeout, TcpStream::connect(&target_addr)).await {
            Ok(Ok(stream)) => {
                // 连接成功
                let connect_time = start_time.elapsed();
                debug!("成功创建到 {} 的连接，耗时: {:?}", target_addr, connect_time);
                
                // 更新评分数据
                if let Some(mut score_data) = score_board.get_mut(&ip) {
                    score_data.record_success(connect_time);
                }
                
                // 尝试将连接添加到池中
                if let Err(_) = pool_state.sender.try_send(stream) {
                    // 发送失败，可能是因为池已满或接收端已关闭
                    // 这种情况下连接会自动关闭
                    debug!("无法将连接添加到池中，可能池已满: {}", target_addr);
                }
            }
            Ok(Err(e)) => {
                // 连接失败
                warn!("连接到 {} 失败: {}", target_addr, e);
                
                // 更新评分数据
                if let Some(mut score_data) = score_board.get_mut(&ip) {
                    score_data.record_failure();
                }
                
                // 连接失败时等待更长时间再重试
                sleep(fill_interval * 2).await;
                continue;
            }
            Err(_) => {
                // 连接超时
                warn!("连接到 {} 超时", target_addr);
                
                // 更新评分数据
                if let Some(mut score_data) = score_board.get_mut(&ip) {
                    score_data.record_failure();
                }
                
                // 超时时等待更长时间再重试
                sleep(fill_interval * 2).await;
                continue;
            }
        }
        
        // 成功创建连接后，短暂等待再创建下一个
        sleep(fill_interval).await;
    }
    
    Ok(())
}

/// 从连接池获取一个连接
/// 
/// 如果池中有可用连接，立即返回；否则返回None
pub async fn get_connection_from_pool(
    pool_manager: &PoolManager,
    ip: IpAddr,
) -> Option<TcpStream> {
    if let Some(pool_state) = pool_manager.get(&ip) {
        let mut receiver = pool_state.receiver.lock().await;
        match receiver.try_recv() {
            Ok(stream) => {
                debug!("从连接池获取到 {} 的连接", ip);
                Some(stream)
            }
            Err(_) => {
                debug!("连接池中没有可用的 {} 连接", ip);
                None
            }
        }
    } else {
        debug!("IP {} 没有对应的连接池", ip);
        None
    }
}
