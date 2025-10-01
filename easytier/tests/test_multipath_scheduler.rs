//! 独立测试多路径调度器
//! 
//! 此文件可独立编译测试调度器逻辑，不依赖项目其他部分

use easytier::peers::multipath_scheduler::*;

/// 创建测试用的路径快照
fn create_test_path(id: PathId, srtt_ms: f32, bw_mbps: f64, loss_rate: f32) -> PathSnapshot {
    PathSnapshot {
        id,
        srtt_ms,
        rttvar_ms: srtt_ms * 0.2,
        min_rtt_ms: srtt_ms * 0.8,
        bw_est_bps: bw_mbps * 1_000_000.0,
        loss_rate,
        inflight_bytes: 0,
        last_tx_age_ms: 0,
        active: true,
    }
}

#[test]
fn test_short_flow_redundancy_detailed() {
    let mut cfg = Config::default();
    cfg.enable_redundancy = true;
    cfg.short_flow_thresh = 64 * 1024;
    cfg.blest_delta_cap_ms = 25;
    let scheduler = Scheduler::new(cfg);
    
    // 创建两条差异明显的路径
    let path1 = create_test_path(1, 50.0, 10.0, 0.0);   // 快路
    let path2 = create_test_path(2, 150.0, 5.0, 0.0);   // 慢路，但 ETA 差距应足够大

    let input = PickInput {
        now_ms: 1000,
        packet_len: 1500,
        flow_bytes_sent: 1024,  // 短流（小于 64KB）
        critical: false,
        paths: vec![path1.clone(), path2.clone()],
    };

    // 手动计算 ETA 差距
    let eta1 = path1.estimate_eta(input.now_ms, input.packet_len);
    let eta2 = path2.estimate_eta(input.now_ms, input.packet_len);
    let eta_gap = eta2 - eta1;
    
    println!("Path1 ETA: {:.2}ms, Path2 ETA: {:.2}ms, Gap: {:.2}ms", eta1, eta2, eta_gap);
    
    // BLEST delta: 0.25 * 50ms = 12.5ms
    let delta = 0.25 * 50.0;
    println!("BLEST delta: {:.2}ms", delta);

    let decision = scheduler.pick_path(&input).unwrap();
    println!("Decision: primary={}, redundant={:?}", decision.primary, decision.redundant);
    
    assert_eq!(decision.primary, 1, "应选择快路作为主路径");
    
    // 如果 ETA 差距 > delta，应该触发冗余
    if eta_gap > delta {
        assert!(decision.redundant.is_some(), "短流 + ETA差距大应该触发冗余");
        assert_eq!(decision.redundant.unwrap(), 2, "冗余路径应该是路径2");
    }
}

#[test]
fn test_idle_burst_redundancy_detailed() {
    let mut cfg = Config::default();
    cfg.enable_redundancy = true;
    cfg.idle_burst_ms = 250;
    cfg.blest_delta_cap_ms = 25;
    let idle_threshold = cfg.idle_burst_ms;
    let scheduler = Scheduler::new(cfg);
    
    let mut path1 = create_test_path(1, 50.0, 10.0, 0.0);
    path1.last_tx_age_ms = 300;  // 超过 idle_burst_ms
    
    let path2 = create_test_path(2, 150.0, 5.0, 0.0);

    let input = PickInput {
        now_ms: 1000,
        packet_len: 1500,
        flow_bytes_sent: 1_000_000,  // 长流
        critical: false,
        paths: vec![path1.clone(), path2.clone()],
    };

    let eta1 = path1.estimate_eta(input.now_ms, input.packet_len);
    let eta2 = path2.estimate_eta(input.now_ms, input.packet_len);
    let eta_gap = eta2 - eta1;
    let delta = 0.25 * 50.0;
    
    println!("Idle burst test:");
    println!("Path1 last_tx_age: {}ms (threshold: {}ms)", path1.last_tx_age_ms, idle_threshold);
    println!("ETA gap: {:.2}ms, delta: {:.2}ms", eta_gap, delta);

    let decision = scheduler.pick_path(&input).unwrap();
    println!("Decision: primary={}, redundant={:?}", decision.primary, decision.redundant);
    
    // 检查空闲条件是否被正确识别
    let is_idle = path1.last_tx_age_ms >= idle_threshold;
    assert!(is_idle, "应该识别为空闲突发");
    
    if eta_gap > delta {
        assert!(decision.redundant.is_some(), "空闲突发 + ETA差距大应该触发冗余");
    }
}

#[test]
fn test_critical_packet_redundancy_detailed() {
    let mut cfg = Config::default();
    cfg.enable_redundancy = true;
    cfg.blest_delta_cap_ms = 25;
    let scheduler = Scheduler::new(cfg);
    
    let path1 = create_test_path(1, 50.0, 10.0, 0.0);
    let path2 = create_test_path(2, 150.0, 5.0, 0.0);

    let input = PickInput {
        now_ms: 1000,
        packet_len: 1500,
        flow_bytes_sent: 1_000_000,  // 长流
        critical: true,              // 关键包
        paths: vec![path1.clone(), path2.clone()],
    };

    let eta1 = path1.estimate_eta(input.now_ms, input.packet_len);
    let eta2 = path2.estimate_eta(input.now_ms, input.packet_len);
    let eta_gap = eta2 - eta1;
    let delta = 0.25 * 50.0;
    
    println!("Critical packet test:");
    println!("ETA gap: {:.2}ms, delta: {:.2}ms", eta_gap, delta);

    let decision = scheduler.pick_path(&input).unwrap();
    println!("Decision: primary={}, redundant={:?}", decision.primary, decision.redundant);
    
    assert!(input.critical, "应该是关键包");
    
    if eta_gap > delta {
        assert!(decision.redundant.is_some(), "关键包 + ETA差距大应该触发冗余");
    }
}

#[test]
fn test_redundancy_not_triggered_when_paths_similar() {
    let mut cfg = Config::default();
    cfg.enable_redundancy = true;
    cfg.short_flow_thresh = 64 * 1024;
    let scheduler = Scheduler::new(cfg);
    
    // 两条非常相似的路径
    let path1 = create_test_path(1, 50.0, 10.0, 0.0);
    let path2 = create_test_path(2, 52.0, 9.5, 0.0);

    let input = PickInput {
        now_ms: 1000,
        packet_len: 1500,
        flow_bytes_sent: 1024,  // 短流
        critical: false,
        paths: vec![path1.clone(), path2.clone()],
    };

    let eta1 = path1.estimate_eta(input.now_ms, input.packet_len);
    let eta2 = path2.estimate_eta(input.now_ms, input.packet_len);
    let eta_gap = (eta2 - eta1).abs();
    let delta = 0.25 * 50.0;
    
    println!("Similar paths test:");
    println!("ETA gap: {:.2}ms, delta: {:.2}ms", eta_gap, delta);

    let decision = scheduler.pick_path(&input).unwrap();
    println!("Decision: primary={}, redundant={:?}", decision.primary, decision.redundant);
    
    // 如果差距小，不应触发冗余
    if eta_gap <= delta {
        assert!(decision.redundant.is_none(), "路径相似时不应触发冗余");
    }
}

#[test]
fn test_long_flow_no_redundancy() {
    let mut cfg = Config::default();
    cfg.enable_redundancy = true;
    cfg.short_flow_thresh = 64 * 1024;
    let short_thresh = cfg.short_flow_thresh;
    let idle_thresh = cfg.idle_burst_ms;
    let scheduler = Scheduler::new(cfg);
    
    let path1 = create_test_path(1, 50.0, 10.0, 0.0);
    let path2 = create_test_path(2, 150.0, 5.0, 0.0);

    let input = PickInput {
        now_ms: 1000,
        packet_len: 1500,
        flow_bytes_sent: 1_000_000,  // 长流（超过 short_flow_thresh）
        critical: false,
        paths: vec![path1.clone(), path2.clone()],
    };

    // 检查没有任何冗余触发条件
    let is_short = input.flow_bytes_sent <= short_thresh;
    let is_idle = input.paths.iter().any(|p| p.last_tx_age_ms >= idle_thresh);
    
    println!("Long flow test:");
    println!("is_short: {}, is_idle: {}, critical: {}", is_short, is_idle, input.critical);

    let decision = scheduler.pick_path(&input).unwrap();
    println!("Decision: primary={}, redundant={:?}", decision.primary, decision.redundant);
    
    // 长流 + 非空闲 + 非关键包 = 不应触发冗余
    if !is_short && !is_idle && !input.critical {
        assert!(decision.redundant.is_none(), "长流非空闲非关键包不应触发冗余");
    }
}
