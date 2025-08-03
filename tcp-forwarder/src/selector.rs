use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::config::SelectorConfig;
use crate::metrics::METRICS;
use crate::scorer::ScoreBoard;

/// 活跃远程地址集合，用于存储当前被选为最优的IP地址列表
pub type ActiveRemotes = Arc<RwLock<Vec<IpAddr>>>;

/// IP变更事件类型，用于记录IP集合的变化
#[derive(Debug)]
struct IpChangeEvent {
    /// 新添加的IP地址列表
    added: Vec<IpAddr>,
    /// 被移除的IP地址列表
    removed: Vec<IpAddr>,
    /// 保持不变的IP地址列表
    unchanged: Vec<IpAddr>,
}

/// 选择器任务，周期性地评估所有IP的分数并更新活跃IP列表
///
/// # 参数
///
/// * `score_board` - 全局共享的IP评分板
/// * `active_remotes` - 全局共享的活跃IP列表
/// * `config` - 选择器配置
pub async fn selector_task(
    score_board: ScoreBoard,
    active_remotes: ActiveRemotes,
    config: SelectorConfig,
) -> Result<()> {
    // 创建周期性定时器，间隔由配置决定
    let mut interval_timer = interval(config.evaluation_interval);

    // 记录上次评估时间，用于实现防抖策略
    let mut last_change_time = Instant::now();

    info!(
        "选择器任务已启动，评估间隔: {:?}",
        config.evaluation_interval
    );

    // 在启动时立即执行一次评估，不等待，确保在TCP服务器启动前就有活跃IP
    debug!("执行启动时的初始IP选择评估");
    match evaluate_and_select(
        &score_board,
        &active_remotes,
        &config,
        &mut last_change_time,
    )
    .await
    {
        Ok(change_event) => {
            if !change_event.added.is_empty() || !change_event.removed.is_empty() {
                info!(
                    "初始活跃IP列表已设置: 添加 {} 个, 移除 {} 个, 保持 {} 个",
                    change_event.added.len(),
                    change_event.removed.len(),
                    change_event.unchanged.len()
                );
                debug!("添加: {:?}", change_event.added);
                debug!("移除: {:?}", change_event.removed);

                // 记录活跃IP数量指标
                let total_active = change_event.added.len() + change_event.unchanged.len();
                METRICS.record_active_ips(total_active as f64);
            } else {
                info!("初始活跃IP设置: {} 个IP", change_event.unchanged.len());
                if !change_event.unchanged.is_empty() {
                    METRICS.record_active_ips(change_event.unchanged.len() as f64);
                }
            }
        }
        Err(e) => {
            warn!("初始IP评估时发生错误: {}", e);
        }
    }

    // 等待2秒让系统稳定，然后开始周期性评估
    tokio::time::sleep(Duration::from_secs(2)).await;

    // match evaluate_and_select(&score_board, &active_remotes, &config, &mut last_change_time).await {
    //     Ok(change_event) => {
    //         info!(
    //             "初始活跃IP列表已设置: 添加 {} 个IP",
    //             change_event.added.len()
    //         );
    //         debug!("初始活跃IP: {:?}", change_event.added);

    //         // 记录活跃IP数量指标
    //         METRICS.record_active_ips(change_event.added.len() as f64);
    //     }
    //     Err(e) => {
    //         warn!("初始IP评估时发生错误: {}", e);
    //     }
    // }

    // 周期性执行选择逻辑
    loop {
        // 执行评估和选择逻辑
        match evaluate_and_select(
            &score_board,
            &active_remotes,
            &config,
            &mut last_change_time,
        )
        .await
        {
            Ok(change_event) => {
                if !change_event.added.is_empty() || !change_event.removed.is_empty() {
                    info!(
                        "活跃IP列表已更新: 添加 {} 个, 移除 {} 个, 保持 {} 个",
                        change_event.added.len(),
                        change_event.removed.len(),
                        change_event.unchanged.len()
                    );
                    debug!("添加: {:?}", change_event.added);
                    debug!("移除: {:?}", change_event.removed);

                    // 记录活跃IP数量指标
                    let total_active = change_event.added.len() + change_event.unchanged.len();
                    METRICS.record_active_ips(total_active as f64);
                } else {
                    debug!(
                        "活跃IP列表未变化，当前有 {} 个活跃IP",
                        change_event.unchanged.len()
                    );
                }
            }
            Err(e) => {
                warn!("评估IP时发生错误: {}", e);
            }
        }

        // 等待下一个时间间隔
        interval_timer.tick().await;
    }
}

/// 评估所有IP并选择最优的一组IP作为活跃列表
///
/// # 参数
///
/// * `score_board` - IP评分板
/// * `active_remotes` - 活跃IP列表
/// * `config` - 选择器配置
/// * `last_change_time` - 上次更改活跃列表的时间
async fn evaluate_and_select(
    score_board: &ScoreBoard,
    active_remotes: &ActiveRemotes,
    config: &SelectorConfig,
    last_change_time: &mut Instant,
) -> Result<IpChangeEvent> {
    // 从评分板获取所有IP的最新分数
    let mut scored_ips: Vec<(IpAddr, f64)> = score_board
        .iter()
        .map(|entry| (*entry.key(), entry.value().total_score()))
        .collect();

    // 按分数从高到低排序
    scored_ips.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // 记录前几个IP的分数用于调试
    debug!("IP评分情况（前10个）:");
    for (i, (ip, score)) in scored_ips.iter().take(10).enumerate() {
        debug!("  #{}: {} 分数: {:.2}", i + 1, ip, score);
    }

    // 应用最低分数阈值过滤
    let qualified_ips: Vec<IpAddr> = scored_ips
        .iter()
        .filter(|(_, score)| *score >= config.min_score_threshold)
        .map(|(ip, _)| *ip)
        .collect();

    info!(
        "符合最低分数要求的IP: {}/{} (阈值: {:.1})",
        qualified_ips.len(),
        scored_ips.len(),
        config.min_score_threshold
    );

    // 确定要选择的IP数量，不超过配置的最大数量和合格IP总数
    let target_size = std::cmp::min(config.active_set_size, qualified_ips.len());

    // 选择前N个高分IP
    let candidate_ips: Vec<IpAddr> = qualified_ips.into_iter().take(target_size).collect();

    // 获取当前活跃IP列表
    let current_active = active_remotes.read().await;
    let current_active_set: Vec<IpAddr> = current_active.clone();
    drop(current_active); // 释放读锁

    // 计算需要添加和移除的IP
    let mut added: Vec<IpAddr> = Vec::new();
    let mut removed: Vec<IpAddr> = Vec::new();
    let mut unchanged: Vec<IpAddr> = Vec::new();

    // 找出需要添加的新IP
    for ip in &candidate_ips {
        if !current_active_set.contains(ip) {
            added.push(*ip);
        } else {
            unchanged.push(*ip);
        }
    }

    // 找出需要移除的旧IP
    for ip in &current_active_set {
        if !candidate_ips.contains(ip) {
            removed.push(*ip);
        }
    }

    // 应用防抖策略
    // 如果距离上次变更时间不足防抖时间，且变更不重要，则取消此次变更
    let now = Instant::now();
    let time_since_last_change = now.duration_since(*last_change_time);

    // 如果有变更且配置了防抖
    if (!added.is_empty() || !removed.is_empty())
        && config.switching.debounce_interval > Duration::from_secs(0)
    {
        // 评估变更是否重要（分数差异是否超过阈值）
        let is_significant_change =
            is_change_significant(score_board, &candidate_ips, &current_active_set, config);

        // 如果变更不重要且未超过防抖时间，则取消此次变更
        if !is_significant_change && time_since_last_change < config.switching.debounce_interval {
            debug!(
                "由于防抖策略，取消此次变更 (上次变更: {:?} 前，阈值: {:?})",
                time_since_last_change, config.switching.debounce_interval
            );
            return Ok(IpChangeEvent {
                added: Vec::new(),
                removed: Vec::new(),
                unchanged: current_active_set,
            });
        }
    }

    // 如果存在变更，更新活跃IP列表
    if !added.is_empty() || !removed.is_empty() {
        let mut active = active_remotes.write().await;
        *active = candidate_ips.clone();
        *last_change_time = now; // 更新上次变更时间

        // 更新被选中的IP的最后选择时间
        for ip in &candidate_ips {
            if let Some(mut score_data) = score_board.get_mut(ip) {
                score_data.update_last_selected();
            }
        }
    }

    Ok(IpChangeEvent {
        added,
        removed,
        unchanged,
    })
}

/// 评估IP变更是否足够重要，以便决定是否应该立即切换
///
/// # 参数
///
/// * `score_board` - IP评分板
/// * `new_ips` - 新选择的IP列表
/// * `current_ips` - 当前活跃的IP列表
/// * `config` - 选择器配置
fn is_change_significant(
    score_board: &ScoreBoard,
    new_ips: &[IpAddr],
    current_ips: &[IpAddr],
    config: &SelectorConfig,
) -> bool {
    // 如果配置了强制性更改（阈值为0），则所有变更都视为重要
    if config.switching.score_improvement_threshold <= 0.0 {
        return true;
    }

    // 计算新IP集合的平均分
    let new_avg_score = calculate_avg_score(score_board, new_ips);

    // 计算当前IP集合的平均分
    let current_avg_score = calculate_avg_score(score_board, current_ips);

    // 如果新集合分数显著高于当前集合分数，则视为重要变更
    let improvement = new_avg_score - current_avg_score;
    let is_significant = improvement >= config.switching.score_improvement_threshold;

    debug!(
        "分数变化评估: 新集合 ({:.2}) vs 当前集合 ({:.2}), 差异: {:.2}, 阈值: {:.2}, 判定为{}重要变更",
        new_avg_score,
        current_avg_score,
        improvement,
        config.switching.score_improvement_threshold,
        if is_significant { "" } else { "不" }
    );

    is_significant
}

/// 计算IP集合的平均分数
///
/// # 参数
///
/// * `score_board` - IP评分板
/// * `ips` - 要计算平均分的IP列表
fn calculate_avg_score(score_board: &ScoreBoard, ips: &[IpAddr]) -> f64 {
    if ips.is_empty() {
        return 0.0;
    }

    let total_score: f64 = ips
        .iter()
        .filter_map(|ip| score_board.get(ip).map(|entry| entry.total_score()))
        .sum();

    total_score / ips.len() as f64
}
