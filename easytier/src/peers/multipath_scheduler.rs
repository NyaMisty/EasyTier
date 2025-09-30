//! Online-Hybrid Multipath Scheduler (在线混合多路径调度器)
//!
//! 本模块实现了一个多路径调度策略,结合以下特性:
//! - 在线 per-packet 路径选择
//! - BLEST 乱序预算控制
//! - QAware 目标排队时延控制
//! - 短流/空闲冗余机制
//!
//! 设计用于用户态 L4 自研栈,不依赖内核 BBR 内参。

#![allow(clippy::too_many_arguments)]
use core::cmp::Ordering;
use std::sync::{Arc, RwLock};

/// 路径标识符
pub type PathId = u64;

/// 路径快照 - 从 ACK/计时器侧维护的状态中读取
#[derive(Clone, Debug)]
pub struct PathSnapshot {
    /// 路径唯一标识
    pub id: PathId,
    /// 平滑往返时延 (ms)
    pub srtt_ms: f32,
    /// RTT 方差 (ms)
    pub rttvar_ms: f32,
    /// 最小 RTT (ms)
    pub min_rtt_ms: f32,
    /// 带宽估计 (Bytes/s, 近 1-3s EWMA)
    pub bw_est_bps: f64,
    /// 丢包率 (0.0..1.0)
    pub loss_rate: f32,
    /// 在途字节数 (未确认)
    pub inflight_bytes: u32,
    /// 上次发送已过去时间 (ms)
    pub last_tx_age_ms: u32,
    /// 路径活跃性标志
    pub active: bool,
}

impl PathSnapshot {
    /// 创建一个默认的路径快照用于测试
    pub fn new(id: PathId) -> Self {
        Self {
            id,
            srtt_ms: 50.0,
            rttvar_ms: 10.0,
            min_rtt_ms: 40.0,
            bw_est_bps: 1_000_000.0, // 1 MB/s
            loss_rate: 0.0,
            inflight_bytes: 0,
            last_tx_age_ms: 0,
            active: true,
        }
    }

    /// 检查路径是否可用
    pub fn is_usable(&self) -> bool {
        self.active && self.bw_est_bps > 0.0
    }

    /// 计算队列时延 (ms)
    pub fn queue_delay_ms(&self) -> f64 {
        if self.bw_est_bps <= 0.0 {
            return 0.0;
        }
        (self.inflight_bytes as f64 / self.bw_est_bps) * 1000.0
    }

    /// 计算 BDP (Bandwidth-Delay Product) 余量
    pub fn headroom_bytes(&self) -> f64 {
        let bdp_bytes = self.bw_est_bps * (self.min_rtt_ms as f64 / 1000.0);
        (bdp_bytes - self.inflight_bytes as f64).max(0.0)
    }

    /// 估计包到达时间 (ETA, ms)
    pub fn estimate_eta(&self, now_ms: u64, packet_len: usize) -> f64 {
        let serialize_ms = if self.bw_est_bps > 0.0 {
            (packet_len as f64 / self.bw_est_bps) * 1000.0
        } else {
            0.0
        };
        let queue_delay = self.queue_delay_ms();
        now_ms as f64 + queue_delay + serialize_ms + 0.5 * self.srtt_ms as f64
    }
}

/// 调度器输入参数
#[derive(Clone, Debug)]
pub struct PickInput {
    /// 当前时间戳 (ms)
    pub now_ms: u64,
    /// 包长度 (bytes)
    pub packet_len: usize,
    /// 流累计已发送字节数 (用于短流/长流判断)
    pub flow_bytes_sent: u64,
    /// 是否为关键包 (应用层标记)
    pub critical: bool,
    /// 可用路径快照列表
    pub paths: Vec<PathSnapshot>,
}

impl PickInput {
    /// 创建一个默认的调度输入用于测试
    pub fn new(packet_len: usize, paths: Vec<PathSnapshot>) -> Self {
        Self {
            now_ms: 0,
            packet_len,
            flow_bytes_sent: 0,
            critical: false,
            paths,
        }
    }
}

/// 调度决策结果
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    /// 主路径 ID
    pub primary: PathId,
    /// 冗余路径 ID (可选)
    pub redundant: Option<PathId>,
}

impl Decision {
    /// 创建一个只有主路径的决策
    pub fn primary_only(path_id: PathId) -> Self {
        Self {
            primary: path_id,
            redundant: None,
        }
    }

    /// 创建一个包含冗余路径的决策
    pub fn with_redundant(primary: PathId, redundant: PathId) -> Self {
        Self {
            primary,
            redundant: Some(redundant),
        }
    }
}

/// 调度器配置参数
#[derive(Clone, Debug)]
pub struct Config {
    /// 带宽权重 (w1)
    pub w1_bw: f64,
    /// 余量/时延权重 (w2)
    pub w2_headroom: f64,
    /// 丢包惩罚权重 (w3)
    pub w3_loss: f64,
    /// 超目标队列时延惩罚权重 (w4)
    pub w4_qdelay: f64,
    /// 目标队列时延阈值 (ms)
    pub target_qdelay_ms: u32,
    /// 短流阈值 (bytes, 触发长流前的累计字节数)
    pub short_flow_thresh: u64,
    /// 空闲突发定义 (ms, 超过此时间未发送则视为空闲)
    pub idle_burst_ms: u32,
    /// BLEST 乱序预算上限 (ms)
    pub blest_delta_cap_ms: u32,
    
    // 护栏开关 (用于逐步放量)
    /// 启用 BLEST 乱序预算控制
    pub enable_blest: bool,
    /// 启用 QAware 目标排队时延控制
    pub enable_qaware: bool,
    /// 启用短流/空闲冗余
    pub enable_redundancy: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            w1_bw: 1.0,
            w2_headroom: 0.8,
            w3_loss: 1.2,
            w4_qdelay: 0.05,
            target_qdelay_ms: 10,           // 以太网 5, Wi-Fi/蜂窝 10-15
            short_flow_thresh: 128 * 1024,  // 128KB
            idle_burst_ms: 250,
            blest_delta_cap_ms: 25,
            
            enable_blest: true,
            enable_qaware: true,
            enable_redundancy: true,
        }
    }
}

impl Config {
    /// 创建一个保守配置 (用于初始部署)
    pub fn conservative() -> Self {
        Self {
            enable_blest: false,
            enable_qaware: false,
            enable_redundancy: false,
            ..Default::default()
        }
    }

    /// 创建一个激进配置 (用于低延迟场景)
    pub fn aggressive() -> Self {
        Self {
            w1_bw: 1.5,
            w2_headroom: 1.0,
            w3_loss: 2.0,
            w4_qdelay: 0.1,
            target_qdelay_ms: 5,
            short_flow_thresh: 256 * 1024,  // 256KB
            ..Default::default()
        }
    }
}

/// 内部候选路径结构 (用于打分和排序)
#[derive(Clone, Debug)]
struct Candidate {
    id: PathId,
    srtt_ms: f32,
    queue_delay_ms: f64,
    eta_ms: f64,
    score: f64,
}

/// 多路径调度器
pub struct Scheduler {
    /// 配置参数
    cfg: Arc<RwLock<Config>>,
}

impl Scheduler {
    /// 创建一个新的调度器实例
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg: Arc::new(RwLock::new(cfg)),
        }
    }

    /// 获取当前配置的副本
    pub fn get_config(&self) -> Config {
        self.cfg.read().unwrap().clone()
    }

    /// 更新配置 (支持热更新)
    pub fn update_config(&self, cfg: Config) {
        *self.cfg.write().unwrap() = cfg;
    }

    /// 为当前包选择路径
    ///
    /// # 返回
    /// - `Some(Decision)`: 成功选择路径
    /// - `None`: 没有可用路径
    pub fn pick_path(&self, inp: &PickInput) -> Option<Decision> {
        let cfg = self.cfg.read().unwrap().clone();
        
        tracing::info!(
            "Multipath scheduler input: now_ms={}, packet_len={}, flow_bytes_sent={}, critical={}, paths_count={}",
            inp.now_ms, inp.packet_len, inp.flow_bytes_sent, inp.critical, inp.paths.len()
        );
        
        // 输出所有输入路径的详细信息
        // tracing::debug!("Input paths details:");
        for (idx, p) in inp.paths.iter().enumerate() {
            tracing::info!(
                "  [{}] Path {}: active={}, srtt={:.2}ms, rttvar={:.2}ms, min_rtt={:.2}ms, bw_est={:.2}Mbps, loss_rate={:.4}, inflight={}bytes, last_tx_age={}ms, queue_delay={:.2}ms, headroom={:.2}bytes, usable={}",
                idx, p.id, p.active, p.srtt_ms, p.rttvar_ms, p.min_rtt_ms, 
                p.bw_est_bps / 1_000_000.0, p.loss_rate, p.inflight_bytes, 
                p.last_tx_age_ms, p.queue_delay_ms(), p.headroom_bytes(), p.is_usable()
            );
        }
        
        // Phase A: 过滤不可用路径
        let mut candidates: Vec<Candidate> = inp
            .paths
            .iter()
            .filter(|p| p.is_usable())
            .map(|p| self.make_candidate(&cfg, inp.now_ms, inp.packet_len, p))
            .collect();

        tracing::debug!(
            "Phase A - Filter inactive: {} usable paths from {} total",
            candidates.len(), inp.paths.len()
        );
        
        if candidates.is_empty() {
            tracing::debug!("No usable paths available");
            return None;
        }
        
        // 记录初始候选路径信息
        for c in &candidates {
            tracing::debug!(
                "  Candidate path {}: srtt={:.2}ms, queue_delay={:.2}ms, eta={:.2}ms, score={:.2}",
                c.id, c.srtt_ms, c.queue_delay_ms, c.eta_ms, c.score
            );
        }

        // Phase C: BLEST 乱序预算过滤
        if cfg.enable_blest {
            let before_count = candidates.len();
            candidates = self.apply_blest_filter(&cfg, candidates);
            tracing::debug!(
                "Phase C - BLEST filter: {} paths remain (filtered out {})",
                candidates.len(), before_count - candidates.len()
            );
        }

        if candidates.is_empty() {
            tracing::debug!("No paths after BLEST filter");
            return None;
        }

        // Phase D: QAware 目标排队时延控制
        if cfg.enable_qaware {
            self.apply_qaware_penalty(&cfg, &mut candidates);
            tracing::debug!("Phase D - QAware penalty applied (target_qdelay={}ms)", cfg.target_qdelay_ms);
            for c in &candidates {
                tracing::debug!(
                    "  Path {} after QAware: score={:.2}, queue_delay={:.2}ms",
                    c.id, c.score, c.queue_delay_ms
                );
            }
        }

        // Phase B: 按分数排序,分数相近用 ETA 破平
        candidates.sort_by(|a, b| match b.score.total_cmp(&a.score) {
            Ordering::Equal => a.eta_ms.total_cmp(&b.eta_ms),
            other => other,
        });

        tracing::debug!("Phase B - Sorted by score:");
        for (i, c) in candidates.iter().enumerate() {
            tracing::debug!(
                "  #{} Path {}: score={:.2}, eta={:.2}ms, srtt={:.2}ms",
                i + 1, c.id, c.score, c.eta_ms, c.srtt_ms
            );
        }

        let primary = candidates[0].id;

        // Phase E: 短流/空闲冗余
        let redundant = if cfg.enable_redundancy {
            let result = self.decide_redundancy(&cfg, inp, &candidates);
            if let Some(redundant_id) = result {
                tracing::debug!(
                    "Phase E - Redundancy enabled: redundant_path={}",
                    redundant_id
                );
            } else {
                tracing::debug!("Phase E - Redundancy not triggered");
            }
            result
        } else {
            tracing::debug!("Phase E - Redundancy disabled");
            None
        };

        let decision = Decision { primary, redundant };
        tracing::debug!(
            "Final decision: primary={}, redundant={:?}",
            decision.primary, decision.redundant
        );
        
        Some(decision)
    }

    /// 创建候选路径 (计算基础分数)
    fn make_candidate(
        &self,
        cfg: &Config,
        now_ms: u64,
        packet_len: usize,
        path: &PathSnapshot,
    ) -> Candidate {
        let bw = path.bw_est_bps.max(1.0);
        let queue_delay_ms = path.queue_delay_ms();
        let eta_ms = path.estimate_eta(now_ms, packet_len);
        let headroom = path.headroom_bytes();

        // Phase B: 在线打分
        let mut score = 0.0;
        score += cfg.w1_bw * bw;
        score += cfg.w2_headroom * (headroom / (path.srtt_ms as f64).max(1.0));
        score -= cfg.w3_loss * (path.loss_rate as f64);

        // 基础队列时延惩罚
        let over_qdelay = (queue_delay_ms - cfg.target_qdelay_ms as f64).max(0.0);
        score -= cfg.w4_qdelay * over_qdelay;

        Candidate {
            id: path.id,
            srtt_ms: path.srtt_ms,
            queue_delay_ms,
            eta_ms,
            score,
        }
    }

    /// 应用 BLEST 乱序预算过滤
    fn apply_blest_filter(&self, cfg: &Config, mut candidates: Vec<Candidate>) -> Vec<Candidate> {
        if candidates.is_empty() {
            return candidates;
        }

        // 按 ETA 排序找到最快路径
        candidates.sort_by(|a, b| a.eta_ms.total_cmp(&b.eta_ms));
        let eta_fast = candidates[0].eta_ms;
        let srtt_fast = candidates[0].srtt_ms;

        // 计算乱序预算 Δ = clamp(0.25*srtt_fast, 4..25ms)
        let delta_ms = (0.25 * srtt_fast as f64)
            .clamp(4.0, cfg.blest_delta_cap_ms as f64);

        tracing::debug!(
            "BLEST filter: eta_fast={:.2}ms, srtt_fast={:.2}ms, delta={:.2}ms",
            eta_fast, srtt_fast, delta_ms
        );

        // 保存第一个候选用于保底
        let first_candidate = candidates[0].clone();

        // 过滤掉 ETA 超出预算的路径
        let filtered: Vec<Candidate> = candidates
            .into_iter()
            .enumerate()
            .filter_map(|(_i, c)| {
                let eta_diff = c.eta_ms - eta_fast;
                let pass = c.eta_ms <= eta_fast + delta_ms + 1e-6;
                tracing::debug!(
                    "  Path {}: eta={:.2}ms, eta_diff={:.2}ms, {} BLEST filter",
                    c.id, c.eta_ms, eta_diff, if pass { "PASS" } else { "FILTERED by" }
                );
                if pass { Some(c) } else { None }
            })
            .collect();

        // 如果全被过滤,保底取最小 ETA
        if filtered.is_empty() {
            tracing::debug!("All paths filtered, keeping fastest path {} as fallback", first_candidate.id);
            vec![first_candidate]
        } else {
            filtered
        }
    }

    /// 应用 QAware 目标排队时延惩罚
    fn apply_qaware_penalty(&self, cfg: &Config, candidates: &mut [Candidate]) {
        let any_under_target = candidates
            .iter()
            .any(|c| c.queue_delay_ms <= cfg.target_qdelay_ms as f64);

        if !any_under_target {
            // 如果全部超标,不额外惩罚,让基础分数决定
            tracing::debug!(
                "QAware: all paths over target ({}ms), no additional penalty",
                cfg.target_qdelay_ms
            );
            return;
        }

        // 对超标路径追加惩罚
        for c in candidates.iter_mut() {
            if c.queue_delay_ms > cfg.target_qdelay_ms as f64 {
                let excess = c.queue_delay_ms - cfg.target_qdelay_ms as f64;
                let penalty = cfg.w4_qdelay * excess;
                let old_score = c.score;
                c.score -= penalty;
                tracing::debug!(
                    "QAware penalty: path {} queue_delay={:.2}ms > target={}ms, excess={:.2}ms, penalty={:.2}, score {:.2} -> {:.2}",
                    c.id, c.queue_delay_ms, cfg.target_qdelay_ms, excess, penalty, old_score, c.score
                );
            }
        }
    }

    /// 决定是否启用冗余发送
    fn decide_redundancy(
        &self,
        cfg: &Config,
        inp: &PickInput,
        candidates: &[Candidate],
    ) -> Option<PathId> {
        if candidates.len() < 2 {
            tracing::debug!("Redundancy: only {} path(s), need at least 2", candidates.len());
            return None;
        }

        // 检查触发条件
        let is_short_flow = inp.flow_bytes_sent <= cfg.short_flow_thresh;
        let is_idle_burst = inp
            .paths
            .iter()
            .any(|p| p.last_tx_age_ms >= cfg.idle_burst_ms);

        tracing::debug!(
            "Redundancy triggers: short_flow={} (sent={}, thresh={}), idle_burst={}, critical={}",
            is_short_flow, inp.flow_bytes_sent, cfg.short_flow_thresh,
            is_idle_burst, inp.critical
        );

        if !is_short_flow && !is_idle_burst && !inp.critical {
            tracing::debug!("Redundancy: no trigger condition met");
            return None;
        }

        // 注意：candidates 已按分数排序，primary 是 candidates[0]
        // 但为了判断冗余，我们需要找到 ETA 次优的路径
        
        // 对候选列表按 ETA 排序以找出最快和次快路径
        let mut eta_sorted = candidates.to_vec();
        eta_sorted.sort_by(|a, b| a.eta_ms.total_cmp(&b.eta_ms));
        
        // 计算 ETA 最快和次快的差距
        let eta_gap = eta_sorted[1].eta_ms - eta_sorted[0].eta_ms;
        
        // 使用 BLEST Δ 作为判断阈值
        let srtt_fast = eta_sorted[0].srtt_ms;
        let delta_ms = (0.25 * srtt_fast as f64)
            .clamp(4.0, cfg.blest_delta_cap_ms as f64);

        tracing::debug!(
            "Redundancy ETA check: fastest={:.2}ms (path {}), 2nd={:.2}ms (path {}), gap={:.2}ms, delta={:.2}ms",
            eta_sorted[0].eta_ms, eta_sorted[0].id,
            eta_sorted[1].eta_ms, eta_sorted[1].id,
            eta_gap, delta_ms
        );

        // 如果差距足够大，值得冗余
        if eta_gap > delta_ms {
            // 返回 ETA 次快的路径作为冗余路径
            // 但要确保它不是主路径
            let primary_id = candidates[0].id;
            if eta_sorted[1].id != primary_id {
                tracing::debug!(
                    "Redundancy: gap ({:.2}ms) > delta ({:.2}ms), selecting path {} as redundant",
                    eta_gap, delta_ms, eta_sorted[1].id
                );
                Some(eta_sorted[1].id)
            } else {
                // 如果 ETA 次快就是主路径，取第三快的（如果存在）
                if eta_sorted.len() > 2 && eta_sorted[2].eta_ms - eta_sorted[0].eta_ms > delta_ms {
                    tracing::debug!(
                        "Redundancy: 2nd fastest is primary, using 3rd fastest path {} as redundant",
                        eta_sorted[2].id
                    );
                    Some(eta_sorted[2].id)
                } else {
                    tracing::debug!("Redundancy: 2nd fastest is primary and no suitable 3rd path");
                    None
                }
            }
        } else {
            tracing::debug!(
                "Redundancy: gap ({:.2}ms) <= delta ({:.2}ms), not worth redundant send",
                eta_gap, delta_ms
            );
            None
        }
    }

    /// 收集调度器统计信息
    pub fn stats(&self) -> SchedulerStats {
        SchedulerStats {
            config: self.get_config(),
        }
    }
}

/// 调度器统计信息
#[derive(Clone, Debug)]
pub struct SchedulerStats {
    pub config: Config,
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_phase_a_filter_inactive() {
        let scheduler = Scheduler::new(Config::default());
        
        let mut path1 = create_test_path(1, 50.0, 1.0, 0.0);
        path1.active = false;
        let path2 = create_test_path(2, 100.0, 0.5, 0.0);

        let input = PickInput {
            now_ms: 0,
            packet_len: 1500,
            flow_bytes_sent: 0,
            critical: false,
            paths: vec![path1, path2],
        };

        let decision = scheduler.pick_path(&input).unwrap();
        assert_eq!(decision.primary, 2, "应该选择活跃路径");
    }

    #[test]
    fn test_phase_b_bandwidth_preference() {
        let scheduler = Scheduler::new(Config::default());
        
        let path1 = create_test_path(1, 50.0, 10.0, 0.0);  // 高带宽
        let path2 = create_test_path(2, 40.0, 1.0, 0.0);   // 低带宽

        let input = PickInput {
            now_ms: 0,
            packet_len: 1500,
            flow_bytes_sent: 0,
            critical: false,
            paths: vec![path1, path2],
        };

        let decision = scheduler.pick_path(&input).unwrap();
        // 由于 path1 带宽显著更高,即使 RTT 稍差,也应该被选中
        assert_eq!(decision.primary, 1, "应该优先选择高带宽路径");
    }

    #[test]
    fn test_phase_b_loss_penalty() {
        let scheduler = Scheduler::new(Config::default());
        
        let path1 = create_test_path(1, 50.0, 5.0, 0.1);   // 10% 丢包
        let path2 = create_test_path(2, 50.0, 5.0, 0.0);   // 无丢包

        let input = PickInput {
            now_ms: 0,
            packet_len: 1500,
            flow_bytes_sent: 0,
            critical: false,
            paths: vec![path1, path2],
        };

        let decision = scheduler.pick_path(&input).unwrap();
        assert_eq!(decision.primary, 2, "应该避免高丢包路径");
    }

    #[test]
    fn test_phase_c_blest_filter() {
        let mut cfg = Config::default();
        cfg.enable_blest = true;
        let scheduler = Scheduler::new(cfg);
        
        let path1 = create_test_path(1, 50.0, 1.0, 0.0);   // 快路
        let path2 = create_test_path(2, 200.0, 1.0, 0.0);  // 慢路

        let input = PickInput {
            now_ms: 0,
            packet_len: 1500,
            flow_bytes_sent: 0,
            critical: false,
            paths: vec![path1, path2],
        };

        let decision = scheduler.pick_path(&input).unwrap();
        // BLEST 应该过滤掉慢路
        assert_eq!(decision.primary, 1);
        assert_eq!(decision.redundant, None, "慢路应该被 BLEST 过滤");
    }

    #[test]
    fn test_phase_d_qaware_penalty() {
        let mut cfg = Config::default();
        cfg.enable_qaware = true;
        cfg.target_qdelay_ms = 10;
        let scheduler = Scheduler::new(cfg);
        
        let mut path1 = create_test_path(1, 50.0, 1.0, 0.0);
        path1.inflight_bytes = 100_000;  // 高在途,高队列时延
        let path2 = create_test_path(2, 60.0, 1.0, 0.0);

        let input = PickInput {
            now_ms: 0,
            packet_len: 1500,
            flow_bytes_sent: 0,
            critical: false,
            paths: vec![path1, path2],
        };

        let decision = scheduler.pick_path(&input).unwrap();
        // path1 队列时延过高,应该被惩罚
        assert_eq!(decision.primary, 2, "应该避免高队列时延路径");
    }

    #[test]
    fn test_phase_e_short_flow_redundancy() {
        let mut cfg = Config::default();
        cfg.enable_redundancy = true;
        cfg.short_flow_thresh = 64 * 1024;
        let scheduler = Scheduler::new(cfg.clone());
        
        let path1 = create_test_path(1, 50.0, 1.0, 0.0);
        let path2 = create_test_path(2, 100.0, 1.0, 0.0);

        let input = PickInput {
            now_ms: 0,
            packet_len: 1500,
            flow_bytes_sent: 1024,  // 短流
            critical: false,
            paths: vec![path1.clone(), path2.clone()],
        };

        // 计算预期
        let eta1 = path1.estimate_eta(input.now_ms, input.packet_len);
        let eta2 = path2.estimate_eta(input.now_ms, input.packet_len);
        let eta_gap = (eta2 - eta1).abs();
        let delta = (0.25_f64 * 50.0).clamp(4.0, cfg.blest_delta_cap_ms as f64);
        
        eprintln!("短流冗余测试:");
        eprintln!("  Path1 ETA: {:.2}ms, Path2 ETA: {:.2}ms", eta1, eta2);
        eprintln!("  ETA Gap: {:.2}ms, BLEST Delta: {:.2}ms", eta_gap, delta);
        eprintln!("  flow_bytes_sent: {}, thresh: {}", input.flow_bytes_sent, cfg.short_flow_thresh);

        let decision = scheduler.pick_path(&input).unwrap();
        eprintln!("  Decision: primary={}, redundant={:?}", decision.primary, decision.redundant);
        
        assert_eq!(decision.primary, 1);
        
        // 只有当 ETA 差距 > delta 时才应该触发冗余
        if eta_gap > delta {
            assert!(decision.redundant.is_some(), 
                "短流 + ETA差距({:.2}ms) > delta({:.2}ms) 应该触发冗余发送", eta_gap, delta);
        }
    }

    #[test]
    fn test_phase_e_idle_burst_redundancy() {
        let mut cfg = Config::default();
        cfg.enable_redundancy = true;
        cfg.idle_burst_ms = 250;
        let scheduler = Scheduler::new(cfg.clone());
        
        let mut path1 = create_test_path(1, 50.0, 1.0, 0.0);
        path1.last_tx_age_ms = 300;  // 空闲超过阈值
        let path2 = create_test_path(2, 100.0, 1.0, 0.0);

        let input = PickInput {
            now_ms: 0,
            packet_len: 1500,
            flow_bytes_sent: 1_000_000,  // 长流
            critical: false,
            paths: vec![path1.clone(), path2.clone()],
        };

        let eta1 = path1.estimate_eta(input.now_ms, input.packet_len);
        let eta2 = path2.estimate_eta(input.now_ms, input.packet_len);
        let eta_gap = (eta2 - eta1).abs();
        let delta = (0.25_f64 * 50.0).clamp(4.0, cfg.blest_delta_cap_ms as f64);
        
        eprintln!("空闲突发冗余测试:");
        eprintln!("  last_tx_age_ms: {}ms, thresh: {}ms", path1.last_tx_age_ms, cfg.idle_burst_ms);
        eprintln!("  Path1 ETA: {:.2}ms, Path2 ETA: {:.2}ms", eta1, eta2);
        eprintln!("  ETA Gap: {:.2}ms, BLEST Delta: {:.2}ms", eta_gap, delta);

        let decision = scheduler.pick_path(&input).unwrap();
        eprintln!("  Decision: primary={}, redundant={:?}", decision.primary, decision.redundant);
        
        // 只有当 ETA 差距 > delta 时才应该触发冗余
        if eta_gap > delta {
            assert!(decision.redundant.is_some(), 
                "空闲突发 + ETA差距({:.2}ms) > delta({:.2}ms) 应该触发冗余", eta_gap, delta);
        }
    }

    #[test]
    fn test_phase_e_critical_redundancy() {
        let mut cfg = Config::default();
        cfg.enable_redundancy = true;
        let scheduler = Scheduler::new(cfg.clone());
        
        let path1 = create_test_path(1, 50.0, 1.0, 0.0);
        let path2 = create_test_path(2, 100.0, 1.0, 0.0);

        let input = PickInput {
            now_ms: 0,
            packet_len: 1500,
            flow_bytes_sent: 1_000_000,  // 长流
            critical: true,              // 关键包
            paths: vec![path1.clone(), path2.clone()],
        };

        let eta1 = path1.estimate_eta(input.now_ms, input.packet_len);
        let eta2 = path2.estimate_eta(input.now_ms, input.packet_len);
        let eta_gap = (eta2 - eta1).abs();
        let delta = (0.25_f64 * 50.0).clamp(4.0, cfg.blest_delta_cap_ms as f64);
        
        eprintln!("关键包冗余测试:");
        eprintln!("  critical: {}", input.critical);
        eprintln!("  Path1 ETA: {:.2}ms, Path2 ETA: {:.2}ms", eta1, eta2);
        eprintln!("  ETA Gap: {:.2}ms, BLEST Delta: {:.2}ms", eta_gap, delta);

        let decision = scheduler.pick_path(&input).unwrap();
        eprintln!("  Decision: primary={}, redundant={:?}", decision.primary, decision.redundant);
        
        // 只有当 ETA 差距 > delta 时才应该触发冗余
        if eta_gap > delta {
            assert!(decision.redundant.is_some(), 
                "关键包 + ETA差距({:.2}ms) > delta({:.2}ms) 应该触发冗余", eta_gap, delta);
        }
    }

    #[test]
    fn test_no_paths_available() {
        let scheduler = Scheduler::new(Config::default());
        
        let input = PickInput {
            now_ms: 0,
            packet_len: 1500,
            flow_bytes_sent: 0,
            critical: false,
            paths: vec![],
        };

        let decision = scheduler.pick_path(&input);
        assert!(decision.is_none(), "没有路径应返回 None");
    }

    #[test]
    fn test_single_path() {
        let scheduler = Scheduler::new(Config::default());
        
        let path1 = create_test_path(1, 50.0, 1.0, 0.0);

        let input = PickInput {
            now_ms: 0,
            packet_len: 1500,
            flow_bytes_sent: 0,
            critical: false,
            paths: vec![path1],
        };

        let decision = scheduler.pick_path(&input).unwrap();
        assert_eq!(decision.primary, 1);
        assert_eq!(decision.redundant, None, "单路径不应有冗余");
    }

    #[test]
    fn test_config_hot_update() {
        let scheduler = Scheduler::new(Config::default());
        
        let mut new_cfg = Config::default();
        new_cfg.target_qdelay_ms = 20;
        scheduler.update_config(new_cfg);

        let cfg = scheduler.get_config();
        assert_eq!(cfg.target_qdelay_ms, 20, "配置应该热更新成功");
    }

    #[test]
    fn test_conservative_config() {
        let cfg = Config::conservative();
        assert!(!cfg.enable_blest, "保守配置应禁用 BLEST");
        assert!(!cfg.enable_qaware, "保守配置应禁用 QAware");
        assert!(!cfg.enable_redundancy, "保守配置应禁用冗余");
    }

    #[test]
    fn test_aggressive_config() {
        let cfg = Config::aggressive();
        assert_eq!(cfg.target_qdelay_ms, 5, "激进配置应使用更低的目标时延");
        assert!(cfg.w3_loss > 1.5, "激进配置应更严格地惩罚丢包");
    }

    #[test]
    fn test_path_snapshot_helpers() {
        let path = create_test_path(1, 50.0, 1.0, 0.0);
        
        assert!(path.is_usable(), "正常路径应该可用");
        assert!(path.queue_delay_ms() >= 0.0, "队列时延应非负");
        assert!(path.headroom_bytes() >= 0.0, "余量应非负");
        
        let eta = path.estimate_eta(1000, 1500);
        assert!(eta > 1000.0, "ETA 应该在未来");
    }

    #[test]
    fn test_realistic_scenario_250ms_10pct_loss() {
        // 模拟: 250ms / 10% 丢包场景
        let scheduler = Scheduler::new(Config::default());
        
        let path1 = create_test_path(1, 50.0, 10.0, 0.05);   // 快路,低丢包
        let path2 = create_test_path(2, 250.0, 5.0, 0.10);   // 慢路,高丢包

        let input = PickInput {
            now_ms: 0,
            packet_len: 1500,
            flow_bytes_sent: 0,
            critical: false,
            paths: vec![path1, path2],
        };

        let decision = scheduler.pick_path(&input).unwrap();
        // 应该优先快路,BLEST 抑制慢路
        assert_eq!(decision.primary, 1, "应该选择快速低丢包路径");
    }

    #[test]
    fn test_realistic_scenario_50ms_5pct_loss() {
        // 模拟: 50ms / 5% 丢包场景 (两路均衡)
        let scheduler = Scheduler::new(Config::default());
        
        let path1 = create_test_path(1, 50.0, 5.0, 0.05);
        let path2 = create_test_path(2, 50.0, 5.0, 0.05);

        let input = PickInput {
            now_ms: 0,
            packet_len: 1500,
            flow_bytes_sent: 0,
            critical: false,
            paths: vec![path1, path2],
        };

        let decision = scheduler.pick_path(&input);
        // 两路相近,任何一条都可接受
        assert!(decision.is_some(), "应该能选出一条路径");
    }
}
