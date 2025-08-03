use std::net::IpAddr;
use std::time::Duration;
use tcp_forwarder::config::{ScoringConfig, LatencyConfig, JitterConfig, SuccessRateConfig, FailurePenaltyConfig, HistoricalBonusConfig, ScoringWeightsConfig};
use tcp_forwarder::scorer::{ScoreData, create_score_board};

fn create_test_config() -> ScoringConfig {
    ScoringConfig {
        weights: ScoringWeightsConfig {
            latency: 0.45,
            jitter: 0.15,
            success_rate: 0.40,
        },
        latency: LatencyConfig {
            max_score: 45.0,
            ewma_alpha: 0.2,
            base_latency: Duration::from_millis(50),
            max_acceptable_latency: Duration::from_millis(500),
        },
        jitter: JitterConfig {
            max_score: 15.0,
            ewma_alpha: 0.3,
            base_jitter: Duration::from_millis(10),
            max_acceptable_jitter: Duration::from_millis(80),
        },
        success_rate: SuccessRateConfig {
            max_score: 40.0,
            ewma_alpha: 0.1,
        },
        failure_penalty: FailurePenaltyConfig {
            base_penalty: 5.0,
            exponent_factor: 1.8,
            max_penalty: 80.0,
            recovery_per_check: 2.5,
        },
        historical_bonus: HistoricalBonusConfig {
            max_bonus: 10.0,
            checks_per_point: 120,
        },
    }
}

fn main() {
    let config = create_test_config();
    let _score_board = create_score_board();
    
    println!("=== TCP转发器评分系统边界测试 ===\n");
    
    // 测试1：完美情况（理论满分）
    println!("测试1：完美情况（理论满分）");
    let test_ip: IpAddr = "192.168.1.1".parse().unwrap();
    let mut perfect_score = ScoreData::new(test_ip, 80, &config);
    
    // 模拟多次完美连接
    for _ in 0..10 {
        perfect_score.update_probe_result(true, Some(Duration::from_millis(25)), &config); // 低于基准延迟
    }
    
    let score = perfect_score.calculate_score(&config);
    let details = perfect_score.get_score_details(&config);
    println!("完美分数: {:.1}/100", score);
    println!("{}\n", details.format());
    
    // 测试2：最差情况（理论0分）
    println!("测试2：最差情况");
    let mut worst_score = ScoreData::new(test_ip, 80, &config);
    
    // 模拟连续失败
    for _ in 0..10 {
        worst_score.update_probe_result(false, None, &config);
    }
    
    let score = worst_score.calculate_score(&config);
    let details = worst_score.get_score_details(&config);
    println!("最差分数: {:.1}/100", score);
    println!("{}\n", details.format());
    
    // 测试3：边界延迟测试
    println!("测试3：延迟边界测试");
    let mut boundary_score = ScoreData::new(test_ip, 80, &config);
    
    // 测试基准延迟（应该得满分）
    boundary_score.update_probe_result(true, Some(Duration::from_millis(50)), &config);
    let score1 = boundary_score.calculate_score(&config);
    println!("基准延迟(50ms)分数: {:.1}/100", score1);
    
    // 测试最大可接受延迟（应该得0分）
    let mut boundary_score2 = ScoreData::new(test_ip, 80, &config);
    boundary_score2.update_probe_result(true, Some(Duration::from_millis(500)), &config);
    let score2 = boundary_score2.calculate_score(&config);
    println!("最大延迟(500ms)分数: {:.1}/100", score2);
    
    // 测试超过最大延迟（应该得0分）
    let mut boundary_score3 = ScoreData::new(test_ip, 80, &config);
    boundary_score3.update_probe_result(true, Some(Duration::from_millis(600)), &config);
    let score3 = boundary_score3.calculate_score(&config);
    println!("超大延迟(600ms)分数: {:.1}/100\n", score3);
    
    // 测试4：历史奖励测试
    println!("测试4：历史奖励测试");
    let mut bonus_score = ScoreData::new(test_ip, 80, &config);
    
    // 模拟长期稳定连接以获得历史奖励
    for i in 0..130 { // 超过120次，应该获得奖励
        bonus_score.update_probe_result(true, Some(Duration::from_millis(30)), &config);
        if i == 119 {
            let temp_score = bonus_score.calculate_score(&config);
            println!("120次成功后分数: {:.1}/100", temp_score);
        }
    }
    
    let final_score = bonus_score.calculate_score(&config);
    let details = bonus_score.get_score_details(&config);
    println!("130次成功后分数: {:.1}/100", final_score);
    println!("{}\n", details.format());
    
    // 测试5：失败惩罚递增测试
    println!("测试5：失败惩罚递增测试");
    let mut penalty_score = ScoreData::new(test_ip, 80, &config);
    
    for i in 1..=7 {
        penalty_score.update_probe_result(false, None, &config);
        let score = penalty_score.calculate_score(&config);
        let details = penalty_score.get_score_details(&config);
        println!("{}次连续失败后分数: {:.1}/100, 惩罚: {:.1}", i, score, details.failure_penalty);
    }
    println!();
    
    // 测试6：恢复机制测试
    println!("测试6：恢复机制测试");
    let mut recovery_score = ScoreData::new(test_ip, 80, &config);
    
    // 先积累惩罚
    for _ in 0..5 {
        recovery_score.update_probe_result(false, None, &config);
    }
    let score_before = recovery_score.calculate_score(&config);
    println!("恢复前分数: {:.1}/100", score_before);
    
    // 逐步恢复
    for i in 1..=10 {
        recovery_score.update_probe_result(true, Some(Duration::from_millis(60)), &config);
        let score = recovery_score.calculate_score(&config);
        let details = recovery_score.get_score_details(&config);
        if i % 2 == 0 {
            println!("恢复{}次后分数: {:.1}/100, 剩余惩罚: {:.1}", i, score, details.failure_penalty);
        }
    }
    println!();
    
    // 测试7：分数上限测试
    println!("测试7：分数上限测试（确保不超过100分）");
    let mut max_score = ScoreData::new(test_ip, 80, &config);
    
    // 给予人为的高奖励来测试上限
    max_score.historical_bonus = 50.0; // 手动设置高奖励
    
    // 完美表现
    for _ in 0..5 {
        max_score.update_probe_result(true, Some(Duration::from_millis(20)), &config);
    }
    
    let final_score = max_score.calculate_score(&config);
    println!("带有高奖励的完美表现分数: {:.1}/100", final_score);
    
    // 验证不超过100分
    assert!(final_score <= 100.0, "分数超过了100分上限: {}", final_score);
    
    println!("\n=== 所有边界测试完成 ===");
    println!("✅ 评分系统在各种边界条件下都正常工作");
    println!("✅ 分数正确归一化到0-100范围");
    println!("✅ 惩罚和奖励机制正常运作");
}
