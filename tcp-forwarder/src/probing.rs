use crate::config::RemotesConfig;
use crate::scorer::{ScoreBoard, ScoreData};
use crate::metrics::METRICS;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::time::{timeout, interval};
use tracing::{debug, info, warn, error, instrument};

/// 启动时的初始探测：对所有IP进行一次快速探测以获得初始评分
#[instrument(skip(score_board, config))]
pub async fn initial_probing(
    score_board: ScoreBoard,
    config: &RemotesConfig,
) -> anyhow::Result<()> {
    // 检查是否启用初始探测
    let initial_config = match &config.probing.initial {
        Some(cfg) if cfg.enabled => cfg,
        _ => {
            info!("初始探测已禁用，跳过启动时探测");
            return Ok(());
        }
    };
    
    info!("开始启动时的初始IP探测...");
    
    // 获取所有需要探测的IP
    let all_ips: Vec<(IpAddr, ScoreData)> = score_board
        .iter()
        .map(|entry| (*entry.key(), entry.value().clone()))
        .collect();
    
    let total_count = all_ips.len();
    if total_count == 0 {
        warn!("没有可探测的IP地址");
        return Ok(());
    }
    
    info!("总共需要探测 {} 个IP，开始并发探测（最大并发数: {}）", 
          total_count, initial_config.max_concurrency);
    
    // 使用信号量控制并发数量，避免过载
    let max_concurrent = std::cmp::min(initial_config.max_concurrency, total_count);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    
    // 创建所有探测任务
    let mut handles = Vec::with_capacity(total_count);
    
    for (ip, score_data) in all_ips {
        let port = score_data.port;
        let score_board = score_board.clone();
        let config = config.clone();
        let semaphore = semaphore.clone();
        let addr = SocketAddr::new(ip, port);
        
        let handle = tokio::spawn(async move {
            // 获取信号量许可
            let _permit = semaphore.acquire().await.expect("信号量已关闭");
            
            if let Err(e) = probe_single_ip_with_timeout(addr, score_board, &config).await {
                warn!("初始探测IP {:?} 失败: {}", addr, e);
            }
        });
        handles.push(handle);
    }
    
    // 等待所有探测任务完成
    let mut completed = 0;
    for handle in handles {
        let _ = handle.await;
        completed += 1;
        
        // 每完成10%显示进度
        if completed % (total_count / 10).max(1) == 0 {
            debug!("初始探测进度: {}/{} ({:.1}%)", completed, total_count, 
                   (completed as f64 / total_count as f64) * 100.0);
        }
    }
    
    info!("初始IP探测完成，共探测了 {} 个IP", total_count);
    Ok(())
}

/// 探测任务：周期性地探测IP并更新评分
#[instrument(skip(score_board, config))]
pub async fn probing_task(
    score_board: ScoreBoard,
    config: RemotesConfig,
) -> anyhow::Result<()> {
    info!("启动IP探测任务，间隔: {:?}", config.probing.interval);
    
    // 创建一个定时器，按照配置的间隔执行
    let mut timer = interval(config.probing.interval);
    let mut probe_cycle = 0u64;
    
    loop {
        // 等待下一个时间间隔
        timer.tick().await;
        probe_cycle += 1;
        
        // 选择需要探测的IP
        let candidates = select_probe_candidates(&score_board, config.probing.probe_candidate_count);
        
        debug!("本轮选择了 {} 个IP进行探测", candidates.len());
        
        // 每10个周期输出一次详细的评分信息用于调试
        if probe_cycle % 10 == 0 {
            debug!("探测周期 #{}, 开始输出详细评分信息:", probe_cycle);
            for (_ip, score_data) in &candidates {
                let details = score_data.get_score_details(&config.scoring);
                debug!("详细评分: {}", details.format());
            }
        }
        
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

/// 探测单个IP（支持自定义超时）
#[instrument(skip(score_board, config))]
async fn probe_single_ip_with_timeout(
    addr: SocketAddr,
    score_board: ScoreBoard,
    config: &RemotesConfig,
) -> anyhow::Result<()> {
    debug!("开始探测IP: {}", addr);
    
    // 记录探测开始
    let ip_str = addr.ip().to_string();
    METRICS.record_ip_probe(&ip_str);
    
    // 决定使用的超时时间
    let probe_timeout = config.probing.initial
        .as_ref()
        .and_then(|cfg| cfg.timeout)
        .unwrap_or(config.probing.timeout);
    
    let start_time = Instant::now();
    let result = timeout(probe_timeout, TcpStream::connect(addr)).await;
    
    // 处理探测结果
    process_probe_result(addr, result, start_time, score_board, config).await
}

/// 探测单个IP
#[instrument(skip(score_board, config))]
async fn probe_single_ip(
    addr: SocketAddr,
    score_board: ScoreBoard,
    config: &RemotesConfig,
) -> anyhow::Result<()> {
    debug!("开始探测IP: {}", addr);
    
    // 记录探测开始
    let ip_str = addr.ip().to_string();
    METRICS.record_ip_probe(&ip_str);
    
    let start_time = Instant::now();
    let result = timeout(config.probing.timeout, TcpStream::connect(addr)).await;
    
    // 处理探测结果
    process_probe_result(addr, result, start_time, score_board, config).await
}

/// 处理探测结果的公共逻辑
async fn process_probe_result(
    addr: SocketAddr,
    result: Result<Result<TcpStream, std::io::Error>, tokio::time::error::Elapsed>,
    start_time: Instant,
    score_board: ScoreBoard,
    config: &RemotesConfig,
) -> anyhow::Result<()> {
    // 获取此IP的评分数据
    let ip = addr.ip();
    let ip_str = addr.ip().to_string();
    let mut success = false;
    let mut latency = None;
    
    match result {
        Ok(Ok(_)) => {
            // 连接成功
            success = true;
            latency = Some(start_time.elapsed());
            debug!("探测成功: {}, 延迟: {:?}", addr, latency.unwrap());
            
            // 记录指标
            METRICS.record_ip_probe_success(&ip_str);
            METRICS.record_probe_duration(latency.unwrap());
        },
        Ok(Err(e)) => {
            // 连接失败但在超时之前
            warn!("探测失败: {}, 错误: {}", addr, e);
            METRICS.record_ip_probe_failed(&ip_str);
        },
        Err(_) => {
            // 连接超时
            warn!("探测超时: {}", addr);
            METRICS.record_ip_probe_failed(&ip_str);
        }
    }
    
    // 更新评分数据
    if let Some(mut entry) = score_board.get_mut(&ip) {
        entry.update_probe_result(success, latency, &config.scoring);
        
        // 计算并记录新的总分
        let score = entry.calculate_score(&config.scoring);
        
        // 记录IP评分指标
        METRICS.record_ip_score(&ip_str, score);
        
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
