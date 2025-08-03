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
    let score_board = create_score_board();
    
    // 测试IP地址
    let test_ip: IpAddr = "192.168.1.1".parse().unwrap();
    
    // 创建测试用的ScoreData
    let mut score_data = ScoreData::new(test_ip, 80, &config);
    
    println!("=== TCP转发器评分系统测试 ===\n");
    
    // 场景1：理想情况（低延迟、低抖动、高成功率）
    println!("场景1：理想情况");
    score_data.update_probe_result(true, Some(Duration::from_millis(30)), &config);
    score_data.update_probe_result(true, Some(Duration::from_millis(35)), &config);
    score_data.update_probe_result(true, Some(Duration::from_millis(25)), &config);
    
    let score1 = score_data.calculate_score(&config);
    let details1 = score_data.get_score_details(&config);
    println!("总分: {:.1}/100", score1);
    println!("{}\n", details1.format());
    
    // 场景2：中等情况
    println!("场景2：中等情况");
    let mut score_data2 = ScoreData::new(test_ip, 80, &config);
    score_data2.update_probe_result(true, Some(Duration::from_millis(150)), &config);
    score_data2.update_probe_result(true, Some(Duration::from_millis(180)), &config);
    score_data2.update_probe_result(false, None, &config); // 一次失败
    score_data2.update_probe_result(true, Some(Duration::from_millis(120)), &config);
    
    let score2 = score_data2.calculate_score(&config);
    let details2 = score_data2.get_score_details(&config);
    println!("总分: {:.1}/100", score2);
    println!("{}\n", details2.format());
    
    // 场景3：差劲情况（高延迟、高抖动、低成功率）
    println!("场景3：差劲情况");
    let mut score_data3 = ScoreData::new(test_ip, 80, &config);
    score_data3.update_probe_result(false, None, &config);
    score_data3.update_probe_result(false, None, &config);
    score_data3.update_probe_result(true, Some(Duration::from_millis(450)), &config);
    score_data3.update_probe_result(false, None, &config);
    score_data3.update_probe_result(true, Some(Duration::from_millis(400)), &config);
    
    let score3 = score_data3.calculate_score(&config);
    let details3 = score_data3.get_score_details(&config);
    println!("总分: {:.1}/100", score3);
    println!("{}\n", details3.format());
    
    // 场景4：极端情况（超时失败）
    println!("场景4：极端情况（连续失败）");
    let mut score_data4 = ScoreData::new(test_ip, 80, &config);
    for _ in 0..5 {
        score_data4.update_probe_result(false, None, &config);
    }
    
    let score4 = score_data4.calculate_score(&config);
    let details4 = score_data4.get_score_details(&config);
    println!("总分: {:.1}/100", score4);
    println!("{}\n", details4.format());
    
    // 场景5：恢复情况
    println!("场景5：从失败中恢复");
    for _ in 0..3 {
        score_data4.update_probe_result(true, Some(Duration::from_millis(60)), &config);
    }
    
    let score5 = score_data4.calculate_score(&config);
    let details5 = score_data4.get_score_details(&config);
    println!("总分: {:.1}/100", score5);
    println!("{}\n", details5.format());
    
    // 验证分数范围
    println!("=== 分数范围验证 ===");
    assert!(score1 >= 0.0 && score1 <= 100.0, "分数1超出范围: {}", score1);
    assert!(score2 >= 0.0 && score2 <= 100.0, "分数2超出范围: {}", score2);
    assert!(score3 >= 0.0 && score3 <= 100.0, "分数3超出范围: {}", score3);
    assert!(score4 >= 0.0 && score4 <= 100.0, "分数4超出范围: {}", score4);
    assert!(score5 >= 0.0 && score5 <= 100.0, "分数5超出范围: {}", score5);
    
    println!("✅ 所有分数都在0-100范围内");
    println!("✅ 评分系统测试完成！");
}
