//! 多路径调度器演示程序
//!
//! 展示如何使用调度器以及各种场景下的行为

use easytier::peers::multipath_scheduler::*;

fn main() {
    println!("========================================");
    println!("多路径调度器演示");
    println!("========================================\n");

    demo_basic_scheduling();
    demo_short_flow_redundancy();
    demo_idle_burst();
    demo_critical_packet();
    demo_blest_filtering();
    demo_qaware_penalty();
    demo_config_presets();
}

/// 演示基础调度
fn demo_basic_scheduling() {
    println!("📋 演示 1: 基础调度");
    println!("-".repeat(40));
    
    let scheduler = Scheduler::new(Config::default());
    
    let paths = vec![
        PathSnapshot {
            id: 1,
            srtt_ms: 50.0,
            rttvar_ms: 10.0,
            min_rtt_ms: 40.0,
            bw_est_Bps: 10_000_000.0,  // 10 MB/s
            loss_rate: 0.01,            // 1% 丢包
            inflight_bytes: 5000,
            last_tx_age_ms: 10,
            active: true,
        },
        PathSnapshot {
            id: 2,
            srtt_ms: 100.0,
            rttvar_ms: 20.0,
            min_rtt_ms: 80.0,
            bw_est_Bps: 5_000_000.0,   // 5 MB/s
            loss_rate: 0.02,            // 2% 丢包
            inflight_bytes: 3000,
            last_tx_age_ms: 15,
            active: true,
        },
    ];

    let input = PickInput {
        now_ms: 1000,
        packet_len: 1500,
        flow_bytes_sent: 500_000,  // 长流
        critical: false,
        paths: paths.clone(),
    };

    if let Some(decision) = scheduler.pick_path(&input) {
        println!("✓ 选择路径: {}", decision.primary);
        println!("  - 路径 1: RTT={}ms, BW={}MB/s, Loss={}%", 
            paths[0].srtt_ms, paths[0].bw_est_Bps / 1_000_000.0, paths[0].loss_rate * 100.0);
        println!("  - 路径 2: RTT={}ms, BW={}MB/s, Loss={}%", 
            paths[1].srtt_ms, paths[1].bw_est_Bps / 1_000_000.0, paths[1].loss_rate * 100.0);
        println!("  → 选择了路径 {} (更高带宽，更低延迟)\n", decision.primary);
    }
}

/// 演示短流冗余
fn demo_short_flow_redundancy() {
    println!("📋 演示 2: 短流冗余");
    println!("-".repeat(40));
    
    let scheduler = Scheduler::new(Config::default());
    
    let path1 = PathSnapshot {
        id: 1,
        srtt_ms: 50.0,
        rttvar_ms: 10.0,
        min_rtt_ms: 40.0,
        bw_est_Bps: 10_000_000.0,
        loss_rate: 0.0,
        inflight_bytes: 0,
        last_tx_age_ms: 10,
        active: true,
    };
    
    let path2 = PathSnapshot {
        id: 2,
        srtt_ms: 150.0,  // 明显更慢
        rttvar_ms: 30.0,
        min_rtt_ms: 120.0,
        bw_est_Bps: 5_000_000.0,
        loss_rate: 0.0,
        inflight_bytes: 0,
        last_tx_age_ms: 15,
        active: true,
    };

    let input = PickInput {
        now_ms: 1000,
        packet_len: 1500,
        flow_bytes_sent: 1024,  // 短流（1KB < 128KB）
        critical: false,
        paths: vec![path1, path2],
    };

    if let Some(decision) = scheduler.pick_path(&input) {
        println!("✓ 短流场景 (已发送 {} bytes < 128KB)", input.flow_bytes_sent);
        println!("  - 主路径: {}", decision.primary);
        println!("  - 冗余路径: {:?}", decision.redundant);
        
        if decision.redundant.is_some() {
            println!("  → 触发冗余发送以降低尾延迟 ✨\n");
        } else {
            println!("  → ETA 差距不足，未触发冗余\n");
        }
    }
}

/// 演示空闲突发
fn demo_idle_burst() {
    println!("📋 演示 3: 空闲突发冗余");
    println!("-".repeat(40));
    
    let scheduler = Scheduler::new(Config::default());
    
    let mut path1 = PathSnapshot {
        id: 1,
        srtt_ms: 50.0,
        rttvar_ms: 10.0,
        min_rtt_ms: 40.0,
        bw_est_Bps: 10_000_000.0,
        loss_rate: 0.0,
        inflight_bytes: 0,
        last_tx_age_ms: 300,  // 空闲 300ms
        active: true,
    };
    
    let path2 = PathSnapshot {
        id: 2,
        srtt_ms: 150.0,
        rttvar_ms: 30.0,
        min_rtt_ms: 120.0,
        bw_est_Bps: 5_000_000.0,
        loss_rate: 0.0,
        inflight_bytes: 0,
        last_tx_age_ms: 15,
        active: true,
    };

    let input = PickInput {
        now_ms: 1000,
        packet_len: 1500,
        flow_bytes_sent: 1_000_000,  // 长流
        critical: false,
        paths: vec![path1.clone(), path2],
    };

    if let Some(decision) = scheduler.pick_path(&input) {
        println!("✓ 空闲突发场景 (路径 1 已空闲 {}ms > 250ms)", path1.last_tx_age_ms);
        println!("  - 主路径: {}", decision.primary);
        println!("  - 冗余路径: {:?}", decision.redundant);
        
        if decision.redundant.is_some() {
            println!("  → 触发冗余发送以快速恢复流 ✨\n");
        } else {
            println!("  → ETA 差距不足，未触发冗余\n");
        }
    }
}

/// 演示关键包
fn demo_critical_packet() {
    println!("📋 演示 4: 关键包冗余");
    println!("-".repeat(40));
    
    let scheduler = Scheduler::new(Config::default());
    
    let path1 = PathSnapshot {
        id: 1,
        srtt_ms: 50.0,
        rttvar_ms: 10.0,
        min_rtt_ms: 40.0,
        bw_est_Bps: 10_000_000.0,
        loss_rate: 0.0,
        inflight_bytes: 0,
        last_tx_age_ms: 10,
        active: true,
    };
    
    let path2 = PathSnapshot {
        id: 2,
        srtt_ms: 150.0,
        rttvar_ms: 30.0,
        min_rtt_ms: 120.0,
        bw_est_Bps: 5_000_000.0,
        loss_rate: 0.0,
        inflight_bytes: 0,
        last_tx_age_ms: 15,
        active: true,
    };

    let input = PickInput {
        now_ms: 1000,
        packet_len: 1500,
        flow_bytes_sent: 1_000_000,
        critical: true,  // 关键包
        paths: vec![path1, path2],
    };

    if let Some(decision) = scheduler.pick_path(&input) {
        println!("✓ 关键包场景");
        println!("  - 主路径: {}", decision.primary);
        println!("  - 冗余路径: {:?}", decision.redundant);
        
        if decision.redundant.is_some() {
            println!("  → 触发冗余发送以确保可靠送达 ✨\n");
        } else {
            println!("  → ETA 差距不足，未触发冗余\n");
        }
    }
}

/// 演示 BLEST 过滤
fn demo_blest_filtering() {
    println!("📋 演示 5: BLEST 乱序预算过滤");
    println!("-".repeat(40));
    
    let scheduler = Scheduler::new(Config::default());
    
    let path1 = PathSnapshot {
        id: 1,
        srtt_ms: 50.0,
        rttvar_ms: 10.0,
        min_rtt_ms: 40.0,
        bw_est_Bps: 10_000_000.0,
        loss_rate: 0.0,
        inflight_bytes: 0,
        last_tx_age_ms: 10,
        active: true,
    };
    
    let path2 = PathSnapshot {
        id: 2,
        srtt_ms: 250.0,  // 非常慢的路径
        rttvar_ms: 50.0,
        min_rtt_ms: 200.0,
        bw_est_Bps: 20_000_000.0,  // 但带宽很高
        loss_rate: 0.0,
        inflight_bytes: 0,
        last_tx_age_ms: 15,
        active: true,
    };

    let input = PickInput {
        now_ms: 1000,
        packet_len: 1500,
        flow_bytes_sent: 500_000,
        critical: false,
        paths: vec![path1.clone(), path2.clone()],
    };

    if let Some(decision) = scheduler.pick_path(&input) {
        let delta = 0.25 * path1.srtt_ms;
        let eta1 = path1.estimate_eta(input.now_ms, input.packet_len);
        let eta2 = path2.estimate_eta(input.now_ms, input.packet_len);
        
        println!("✓ BLEST 过滤场景");
        println!("  - 路径 1: ETA ≈ {:.1}ms", eta1);
        println!("  - 路径 2: ETA ≈ {:.1}ms (带宽高但延迟大)", eta2);
        println!("  - BLEST Δ = {:.1}ms (0.25 × {} = {})", delta, path1.srtt_ms, delta);
        println!("  - ETA 差距 = {:.1}ms", eta2 - eta1);
        
        if eta2 - eta1 > delta {
            println!("  → 路径 2 被 BLEST 过滤，避免乱序 🚫\n");
        } else {
            println!("  → 路径 2 未被过滤\n");
        }
        println!("  选择路径: {}\n", decision.primary);
    }
}

/// 演示 QAware 惩罚
fn demo_qaware_penalty() {
    println!("📋 演示 6: QAware 队列时延控制");
    println!("-".repeat(40));
    
    let scheduler = Scheduler::new(Config::default());
    
    let mut path1 = PathSnapshot {
        id: 1,
        srtt_ms: 50.0,
        rttvar_ms: 10.0,
        min_rtt_ms: 40.0,
        bw_est_Bps: 10_000_000.0,
        loss_rate: 0.0,
        inflight_bytes: 200_000,  // 高在途
        last_tx_age_ms: 10,
        active: true,
    };
    
    let path2 = PathSnapshot {
        id: 2,
        srtt_ms: 60.0,
        rttvar_ms: 12.0,
        min_rtt_ms: 50.0,
        bw_est_Bps: 8_000_000.0,
        loss_rate: 0.0,
        inflight_bytes: 10_000,   // 低在途
        last_tx_age_ms: 15,
        active: true,
    };

    let input = PickInput {
        now_ms: 1000,
        packet_len: 1500,
        flow_bytes_sent: 500_000,
        critical: false,
        paths: vec![path1.clone(), path2.clone()],
    };

    if let Some(decision) = scheduler.pick_path(&input) {
        let qdelay1 = path1.queue_delay_ms();
        let qdelay2 = path2.queue_delay_ms();
        
        println!("✓ QAware 场景 (目标队列时延: 10ms)");
        println!("  - 路径 1: 队列时延 = {:.1}ms", qdelay1);
        println!("  - 路径 2: 队列时延 = {:.1}ms", qdelay2);
        
        if qdelay1 > 10.0 {
            println!("  → 路径 1 队列时延超标，被惩罚 ⚠️");
        }
        if qdelay2 > 10.0 {
            println!("  → 路径 2 队列时延超标，被惩罚 ⚠️");
        }
        
        println!("  选择路径: {} (更低队列时延)\n", decision.primary);
    }
}

/// 演示配置预设
fn demo_config_presets() {
    println!("📋 演示 7: 配置预设");
    println!("-".repeat(40));
    
    println!("✓ 默认配置:");
    let default_cfg = Config::default();
    println!("  - BLEST: {}", if default_cfg.enable_blest { "✓" } else { "✗" });
    println!("  - QAware: {}", if default_cfg.enable_qaware { "✓" } else { "✗" });
    println!("  - 冗余: {}", if default_cfg.enable_redundancy { "✓" } else { "✗" });
    println!("  - 目标队列时延: {}ms", default_cfg.target_qdelay_ms);
    println!("  - 短流阈值: {}KB\n", default_cfg.short_flow_thresh / 1024);
    
    println!("✓ 保守配置 (初始部署):");
    let conservative_cfg = Config::conservative();
    println!("  - BLEST: {}", if conservative_cfg.enable_blest { "✓" } else { "✗" });
    println!("  - QAware: {}", if conservative_cfg.enable_qaware { "✓" } else { "✗" });
    println!("  - 冗余: {}", if conservative_cfg.enable_redundancy { "✓" } else { "✗" });
    println!("  → 所有护栏禁用，只使用基础打分\n");
    
    println!("✓ 激进配置 (低延迟场景):");
    let aggressive_cfg = Config::aggressive();
    println!("  - 目标队列时延: {}ms (vs 默认 {}ms)", 
        aggressive_cfg.target_qdelay_ms, default_cfg.target_qdelay_ms);
    println!("  - 丢包惩罚权重: {:.1} (vs 默认 {:.1})", 
        aggressive_cfg.w3_loss, default_cfg.w3_loss);
    println!("  → 更严格的延迟和丢包控制\n");
}
