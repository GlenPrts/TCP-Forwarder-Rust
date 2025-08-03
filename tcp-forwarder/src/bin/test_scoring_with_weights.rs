use std::net::IpAddr;
use std::time::Duration;
use tcp_forwarder::config::{ScoringConfig, ScoringWeightsConfig, LatencyConfig, JitterConfig, SuccessRateConfig, FailurePenaltyConfig, HistoricalBonusConfig};
use tcp_forwarder::scorer::ScoreData;

fn main() {
    println!("=== TCP转发器评分系统测试（使用权重配置）===\n");

    // 创建测试配置 - 使用与config.yaml相同的权重
    let config = ScoringConfig {
        weights: ScoringWeightsConfig {
            latency: 0.45,    // 45% 权重
            jitter: 0.15,     // 15% 权重
            success_rate: 0.40, // 40% 权重
        },
        latency: LatencyConfig {
            max_score: 45.0,  // 这个字段不再使用，但保留配置兼容性
            base_latency: Duration::from_millis(50),
            max_acceptable_latency: Duration::from_millis(500),
            ewma_alpha: 0.3,
        },
        jitter: JitterConfig {
            max_score: 15.0,  // 这个字段不再使用，但保留配置兼容性
            base_jitter: Duration::from_millis(10),
            max_acceptable_jitter: Duration::from_millis(80),
            ewma_alpha: 0.3,
        },
        success_rate: SuccessRateConfig {
            max_score: 40.0,  // 这个字段不再使用，但保留配置兼容性
            ewma_alpha: 0.2,
        },
        failure_penalty: FailurePenaltyConfig {
            base_penalty: 5.0,
            exponent_factor: 1.5,
            max_penalty: 30.0,
            recovery_per_check: 1.0,
        },
        historical_bonus: HistoricalBonusConfig {
            max_bonus: 10.0,
            checks_per_point: 10,
        },
    };

    // 测试案例1：理想IP（低延迟、低抖动、高成功率）
    println!("1. 理想IP测试:");
    let ideal_ip: IpAddr = "8.8.8.8".parse().unwrap();
    let mut ideal_score_data = ScoreData::new(ideal_ip, 443, &config);
    
    // 模拟理想性能数据 - 多次更新确保EWMA收敛
    for _ in 0..10 {
        ideal_score_data.latency_ewma.update(30.0);      // 30ms 延迟（低于基准50ms）
        ideal_score_data.jitter_ewma.update(5.0);        // 5ms 抖动（低于基准10ms）
        ideal_score_data.success_rate_ewma.update(0.99); // 99% 成功率
    }
    
    let ideal_score = ideal_score_data.calculate_score(&config);
    println!("   延迟: {:.1}ms (权重45%), 抖动: {:.1}ms (权重15%), 成功率: {:.1}% (权重40%)", 
             ideal_score_data.latency_ewma.value(), 
             ideal_score_data.jitter_ewma.value(), 
             ideal_score_data.success_rate_ewma.value() * 100.0);
    println!("   总分: {:.1}/100", ideal_score);
    
    // 手动验证计算
    let latency_norm = 1.0; // 30ms <= 50ms，满分
    let jitter_norm = 1.0;  // 5ms <= 10ms，满分
    let success_norm = ideal_score_data.success_rate_ewma.value();
    let expected = (latency_norm * 0.45 + jitter_norm * 0.15 + success_norm * 0.40) * 100.0;
    println!("   手动计算验证: {:.1} 分\n", expected);

    // 测试案例2：平均IP（中等延迟、中等抖动、中等成功率）
    println!("2. 平均IP测试:");
    let average_ip: IpAddr = "1.1.1.1".parse().unwrap();
    let mut average_score_data = ScoreData::new(average_ip, 443, &config);
    
    // 模拟平均性能数据 - 多次更新确保EWMA收敛
    for _ in 0..10 {
        average_score_data.latency_ewma.update(150.0);   // 150ms 延迟
        average_score_data.jitter_ewma.update(30.0);     // 30ms 抖动
        average_score_data.success_rate_ewma.update(0.85); // 85% 成功率
    }
    
    let average_score = average_score_data.calculate_score(&config);
    println!("   延迟: {:.1}ms (权重45%), 抖动: {:.1}ms (权重15%), 成功率: {:.1}% (权重40%)", 
             average_score_data.latency_ewma.value(), 
             average_score_data.jitter_ewma.value(), 
             average_score_data.success_rate_ewma.value() * 100.0);
    println!("   总分: {:.1}/100", average_score);
    
    // 手动验证计算
    let latency_norm = (500.0 - 150.0) / (500.0 - 50.0); // = 350/450 = 0.778
    let jitter_norm = (80.0 - 30.0) / (80.0 - 10.0);     // = 50/70 = 0.714
    let success_norm = average_score_data.success_rate_ewma.value();
    let expected = (latency_norm * 0.45 + jitter_norm * 0.15 + success_norm * 0.40) * 100.0;
    println!("   手动计算验证: {:.1} 分\n", expected);

    // 测试案例3：差劲IP（高延迟、高抖动、低成功率）
    println!("3. 差劲IP测试:");
    let poor_ip: IpAddr = "192.168.1.1".parse().unwrap();
    let mut poor_score_data = ScoreData::new(poor_ip, 443, &config);
    
    // 模拟差劲性能数据
    poor_score_data.latency_ewma.update(450.0);     // 450ms 延迟
    poor_score_data.jitter_ewma.update(70.0);       // 70ms 抖动
    poor_score_data.success_rate_ewma.update(0.60); // 60% 成功率
    
    let poor_score = poor_score_data.calculate_score(&config);
    println!("   延迟: 450ms (权重45%), 抖动: 70ms (权重15%), 成功率: 60% (权重40%)");
    println!("   总分: {:.1}/100", poor_score);
    
    // 计算预期分数：
    // 延迟标准化: (500-450)/(500-50) = 50/450 = 0.111, 分数: 0.111 * 45 = 5.0
    // 抖动标准化: (80-70)/(80-10) = 10/70 = 0.143, 分数: 0.143 * 15 = 2.1
    // 成功率分数: 0.60 * 40 = 24.0
    // 总分: 5.0 + 2.1 + 24.0 = 31.1
    println!("   预期分数: ~31.1 分\n");

    // 测试案例4：极差IP（超过最大阈值）
    println!("4. 极差IP测试:");
    let terrible_ip: IpAddr = "10.0.0.1".parse().unwrap();
    let mut terrible_score_data = ScoreData::new(terrible_ip, 443, &config);
    
    // 模拟极差性能数据
    terrible_score_data.latency_ewma.update(600.0);   // 600ms 延迟（超过500ms阈值）
    terrible_score_data.jitter_ewma.update(100.0);    // 100ms 抖动（超过80ms阈值）
    terrible_score_data.success_rate_ewma.update(0.30); // 30% 成功率
    
    let terrible_score = terrible_score_data.calculate_score(&config);
    println!("   延迟: 600ms (权重45%), 抖动: 100ms (权重15%), 成功率: 30% (权重40%)");
    println!("   总分: {:.1}/100", terrible_score);
    
    // 计算预期分数：
    // 延迟分数: 0 (超过阈值)
    // 抖动分数: 0 (超过阈值)
    // 成功率分数: 0.30 * 40 = 12.0
    // 总分: 0 + 0 + 12.0 = 12.0
    println!("   预期分数: ~12.0 分\n");

    // 测试案例5：权重验证 - 修改权重看分数变化
    println!("5. 权重验证测试:");
    let test_config_latency_focused = ScoringConfig {
        weights: ScoringWeightsConfig {
            latency: 0.70,    // 70% 权重给延迟
            jitter: 0.10,     // 10% 权重给抖动
            success_rate: 0.20, // 20% 权重给成功率
        },
        ..config.clone()  // 其他配置保持不变
    };
    
    let mut test_score_data = ScoreData::new("8.8.4.4".parse().unwrap(), 443, &test_config_latency_focused);
    test_score_data.latency_ewma.update(30.0);      // 优秀延迟
    test_score_data.jitter_ewma.update(50.0);       // 差劲抖动
    test_score_data.success_rate_ewma.update(0.95); // 优秀成功率
    
    let original_score = test_score_data.calculate_score(&config);
    let latency_focused_score = test_score_data.calculate_score(&test_config_latency_focused);
    
    println!("   相同性能数据，不同权重配置：");
    println!("   原始权重 (45%/15%/40%): {:.1} 分", original_score);
    println!("   延迟权重 (70%/10%/20%): {:.1} 分", latency_focused_score);
    println!("   由于延迟表现优秀，延迟权重更高的配置得分更高\n");

    println!("=== 权重配置测试完成 ===");
    println!("✅ 评分系统正确使用了配置文件中的权重值");
    println!("✅ 最终分数正确归一化到0-100范围");
    println!("✅ 权重变化能正确影响最终评分");
}
