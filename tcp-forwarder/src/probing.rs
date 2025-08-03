use crate::config::RemotesConfig;
use crate::scorer::{ScoreBoard, ScoreData};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::time::{timeout, interval};
use tracing::{debug, info, warn, error, instrument};

/// 探测任务：周期性地探测IP并更新评分
#[instrument(skip(score_board, config))]
pub async fn probing_task(
    score_board: ScoreBoard,
    config: RemotesConfig,
) -> anyhow::Result<()> {
    info!("启动IP探测任务，间隔: {:?}", config.probing.interval);
    
    // 创建一个定时器，按照配置的间隔执行
    let mut timer = interval(config.probing.interval);
    
    loop {
        // 等待下一个时间间隔
        timer.tick().await;
        
        // 选择需要探测的IP
        let candidates = select_probe_candidates(&score_board, config.probing.probe_candidate_count);
        
        debug!("本轮选择了 {} 个IP进行探测", candidates.len());
        
        // 对每个候选IP进行探测
        for (ip, score_data) in candidates {
            let port = score_data.port;
            // 克隆评分板的引用
            let score_board = score_board.clone();
            let config = config.clone();
            
            // 为每个IP的探测创建单独的任务
            tokio::spawn(async move {
                let addr = SocketAddr::new(ip, port);
                if let Err(e) = probe_single_ip(addr, score_board, &config).await {
                    warn!("探测IP {:?} 失败: {}", addr, e);
                }
            });
        }
    }
    
    // 这个函数永远不会返回，但我们需要返回Result类型以符合函数签名
    #[allow(unreachable_code)]
    Ok(())
}

/// 从评分板中选择需要探测的候选IP
fn select_probe_candidates(
    score_board: &ScoreBoard,
    count: usize,
) -> Vec<(IpAddr, ScoreData)> {
    // 从评分板中获取所有IP
    let mut all_ips: Vec<(IpAddr, ScoreData)> = score_board
        .iter()
        .map(|entry| (*entry.key(), entry.value().clone()))
        .collect();
    
    // 如果IP数量少于需要探测的数量，全部探测
    if all_ips.len() <= count {
        return all_ips;
    }
    
    // 按最后探测时间排序，优先探测最久未探测的IP
    all_ips.sort_by(|a, b| {
        let time_a = a.1.last_probed.unwrap_or_else(|| Instant::now() - Duration::from_secs(3600));
        let time_b = b.1.last_probed.unwrap_or_else(|| Instant::now() - Duration::from_secs(3600));
        time_a.cmp(&time_b)
    });
    
    // 返回需要的数量
    all_ips.truncate(count);
    all_ips
}

/// 探测单个IP
#[instrument(skip(score_board, config))]
async fn probe_single_ip(
    addr: SocketAddr,
    score_board: ScoreBoard,
    config: &RemotesConfig,
) -> anyhow::Result<()> {
    debug!("开始探测IP: {}", addr);
    
    let start_time = Instant::now();
    let result = timeout(config.probing.timeout, TcpStream::connect(addr)).await;
    
    // 获取此IP的评分数据
    let ip = addr.ip();
    let mut success = false;
    let mut latency = None;
    
    match result {
        Ok(Ok(_)) => {
            // 连接成功
            success = true;
            latency = Some(start_time.elapsed());
            debug!("探测成功: {}, 延迟: {:?}", addr, latency.unwrap());
        },
        Ok(Err(e)) => {
            // 连接失败但在超时之前
            warn!("探测失败: {}, 错误: {}", addr, e);
        },
        Err(_) => {
            // 连接超时
            warn!("探测超时: {}", addr);
        }
    }
    
    // 更新评分数据
    if let Some(mut entry) = score_board.get_mut(&ip) {
        entry.update_probe_result(success, latency, &config.scoring);
        
        // 计算并记录新的总分
        let score = entry.calculate_score(&config.scoring);
        debug!(
            "更新IP分数: {}, 成功: {}, 新分数: {:.2}, 延迟: {:?}, 抖动: {:.2}ms", 
            addr, success, score, 
            latency.map(|d| format!("{:.2}ms", d.as_millis())).unwrap_or_else(|| "N/A".to_string()),
            entry.jitter_ewma.value()
        );
    } else {
        // 理论上不应该发生，因为我们是从评分板选择的IP
        error!("探测后无法找到IP: {} 的评分数据", ip);
    }
    
    Ok(())
}
