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

/// 调度器输入参数（旧：按包）
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

/// 调度决策结果（旧：按包）
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

// ---------------- 新增：Epoch 级输入/输出 ----------------

#[derive(Clone, Debug)]
pub struct EpochInput {
    pub now_ms: u64,
    pub epoch_ms: u32,            // 例如 200ms
    pub packet_len_hint: usize,   // 典型包长/MTU
    pub paths: Vec<PathSnapshot>,
    pub prev_weights: Option<Vec<PathWeight>>, // 上一轮权重用于 EMA
}

#[derive(Clone, Debug)]
pub struct PathWeight {
    pub id: PathId,
    pub weight: f64, // 0..1，总和为 1
}

#[derive(Clone, Debug)]
pub enum RedundancyMode {
    Off,
    ShortIdleCritical {
        duplicate_first_n: u8,
        pair: (PathId, PathId),
        budget_pkts: u32,
    },
    Percentage {
        percent: f32,
        pair: (PathId, PathId),
        budget_pkts: u32,
    },
}

#[derive(Clone, Debug)]
pub struct EpochPlan {
    pub valid_until_ms: u64,
    pub lb_weights: Vec<PathWeight>,
    pub redundancy: RedundancyMode,
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

    // 新增：权重化与平滑参数
    /// 丢包惩罚系数 gamma（loss_factor = clamp(1 - gamma*loss, 0.2, 1.0)）
    pub loss_gamma: f64,
    /// QAware 平滑系数 beta
    pub qaware_beta: f64,
    /// BLEST 指数门控时间常数 tau（ms）
    pub blest_tau_ms: f64,
    /// 余量增益强度 k
    pub headroom_k: f64,
    /// 余量增益上限（倍率）
    pub headroom_cap: f64,
    /// EMA 平滑 alpha
    pub smooth_alpha: f64,
    /// 每条最小份额（避免饥饿）
    pub min_share: f64,
    /// 参与冗余的最小权重门槛
    pub redundancy_min_weight: f64,
    /// 每个 epoch 的冗余预算（包数）
    pub redundancy_epoch_budget_pkts: u32,
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

            // 新增默认
            loss_gamma: 1.0,
            qaware_beta: 0.6,
            blest_tau_ms: 20.0,
            headroom_k: 0.5,
            headroom_cap: 1.5,
            smooth_alpha: 0.6,
            min_share: 0.0,
            redundancy_min_weight: 0.05,
            redundancy_epoch_budget_pkts: 32,
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

    /// 为当前 epoch 计算负载分担权重与冗余策略
    ///
    /// - 输入：EpochInput（200ms 节拍推荐）
    /// - 输出：EpochPlan { lb_weights, redundancy }
    ///
    /// 注：保留原有 Phase A-E 的日志结构与注释，内部从“逐包决策”改为“权重化计算”。
    pub fn compute_plan(&self, inp: &EpochInput) -> Option<EpochPlan> {
        let cfg = self.cfg.read().unwrap().clone();
        
        tracing::info!(
            "Multipath scheduler epoch input: now_ms={}, epoch_ms={}, packet_len_hint={}, paths_count={}",
            inp.now_ms, inp.epoch_ms, inp.packet_len_hint, inp.paths.len()
        );
        
        // 输出所有输入路径的详细信息（沿用原日志风格）
        for (idx, p) in inp.paths.iter().enumerate() {
            tracing::info!(
                "  [{}] Path {}: active={}, srtt={:.2}ms, rttvar={:.2}ms, min_rtt={:.2}ms, bw_est={:.2}Mbps, loss_rate={:.4}, inflight={}bytes, last_tx_age={}ms, queue_delay={:.2}ms, headroom={:.2}bytes, usable={}",
                idx, p.id, p.active, p.srtt_ms, p.rttvar_ms, p.min_rtt_ms, 
                p.bw_est_bps / 1_000_000.0, p.loss_rate, p.inflight_bytes, 
                p.last_tx_age_ms, p.queue_delay_ms(), p.headroom_bytes(), p.is_usable()
            );
        }
        
        // Phase A: 过滤不可用路径（轻量）
        let mut candidates: Vec<Candidate> = inp
            .paths
            .iter()
            .filter(|p| p.is_usable())
            .map(|p| self.make_candidate(&cfg, inp.now_ms, inp.packet_len_hint, p))
            .collect();

        tracing::debug!(
            "Phase A - Filter inactive: {} usable paths from {} total",
            candidates.len(), inp.paths.len()
        );
        
        if candidates.is_empty() {
            tracing::debug!("No usable paths available");
            return None;
        }

        if candidates.len() == 1 {
            // 单路径：权重全给存活者，冗余关闭
            let only = candidates[0].id;
            tracing::debug!("Single usable path: {} (assign weight=1.0)", only);
            return Some(EpochPlan {
                valid_until_ms: inp.now_ms + inp.epoch_ms as u64,
                lb_weights: vec![PathWeight { id: only, weight: 1.0 }],
                redundancy: RedundancyMode::Off,
            });
        }
        
        // 记录初始候选路径信息
        for c in &candidates {
            tracing::debug!(
                "  Candidate path {}: srtt={:.2}ms, queue_delay={:.2}ms, eta={:.2}ms",
                c.id, c.srtt_ms, c.queue_delay_ms, c.eta_ms
            );
        }

        // Phase C: BLEST 乱序预算门控（权重化，不做硬过滤）
        // 先按 ETA 找到最快路径与 Δ
        candidates.sort_by(|a, b| a.eta_ms.total_cmp(&b.eta_ms));
        let eta_fast = candidates[0].eta_ms;
        let srtt_fast = candidates[0].srtt_ms;
        let delta_ms = (0.25 * srtt_fast as f64)
            .clamp(4.0, cfg.blest_delta_cap_ms as f64);
        tracing::debug!(
            "Phase C - BLEST gating: eta_fast={:.2}ms, srtt_fast={:.2}ms, delta={:.2}ms",
            eta_fast, srtt_fast, delta_ms
        );

        // Phase D: QAware 目标排队时延控制（平滑罚因子）
        tracing::debug!("Phase D - QAware penalty factors (target_qdelay={}ms)", cfg.target_qdelay_ms);

        // Phase B: 原按分数排序改为计算原始权重 raw_i 并归一化
        // 组合：eff * loss_factor * qaware * blest * headroom_boost
        let mut raws: Vec<(PathId, f64, f64)> = Vec::with_capacity(candidates.len()); // (id, raw, eta)
        for c in &candidates {
            // 基础有效带宽
            let bw = inp
                .paths
                .iter()
                .find(|p| p.id == c.id)
                .map(|p| p.bw_est_bps)
                .unwrap_or(0.0)
                .max(1.0);

            // 丢包惩罚（平滑）
            let loss = inp
                .paths
                .iter()
                .find(|p| p.id == c.id)
                .map(|p| p.loss_rate)
                .unwrap_or(0.0) as f64;
            let loss_factor = (1.0 - cfg.loss_gamma * loss).clamp(0.2, 1.0);

            // QAware 罚因子
            let qaware = if cfg.enable_qaware {
                if c.queue_delay_ms <= cfg.target_qdelay_ms as f64 {
                    1.0
                } else {
                    let over = (c.queue_delay_ms - cfg.target_qdelay_ms as f64)
                        / (cfg.target_qdelay_ms as f64);
                    1.0 / (1.0 + cfg.qaware_beta * over.max(0.0))
                }
            } else {
                1.0
            };

            // BLEST 指数门控
            let blest = if cfg.enable_blest {
                if c.eta_ms <= eta_fast + delta_ms { 1.0 } else {
                    let extra = c.eta_ms - (eta_fast + delta_ms);
                    f64::exp(-(extra / cfg.blest_tau_ms))
                }
            } else { 1.0 };

            // 余量微增益
            let (bdp_bytes, headroom_ratio) = inp
                .paths
                .iter()
                .find(|p| p.id == c.id)
                .map(|p| {
                    let bdp = bw * (p.min_rtt_ms as f64 / 1000.0);
                    let headroom = (bdp - p.inflight_bytes as f64).max(0.0);
                    let ratio = if bdp > 0.0 { headroom / bdp } else { 0.0 };
                    (bdp, ratio)
                })
                .unwrap_or((0.0, 0.0));
            let _ = bdp_bytes; // 仅用于可读性
            let headroom_boost = (1.0 + cfg.headroom_k * headroom_ratio.clamp(0.0, 1.0))
                .min(cfg.headroom_cap);

            let mut raw = bw * loss_factor * qaware * blest * headroom_boost;
            if raw < 1e-9 { raw = 0.0; }

            tracing::debug!(
                "  Path {} factors: bw={:.2}, loss_factor={:.3}, qaware={:.3}, blest={:.3}, headroom_boost={:.3} => raw={:.4}",
                c.id, bw, loss_factor, qaware, blest, headroom_boost, raw
            );

            raws.push((c.id, raw, c.eta_ms));
        }

        // 归一化
        let sum_raw: f64 = raws.iter().map(|(_, r, _)| *r).sum();
        if sum_raw <= 0.0 {
            tracing::debug!("All raw weights ~0, fallback to fastest path weight=1.0");
            let fastest = candidates[0].id;
            return Some(EpochPlan {
                valid_until_ms: inp.now_ms + inp.epoch_ms as u64,
                lb_weights: vec![PathWeight { id: fastest, weight: 1.0 }],
                redundancy: RedundancyMode::Off,
            });
        }
        for (_, r, _) in raws.iter_mut() { *r /= sum_raw; }

        // EMA 平滑 + 最小份额
        let mut weights: Vec<PathWeight> = Vec::with_capacity(raws.len());
        for (id, raw, _eta) in &raws {
            // 若 prev 不存在，直接取 raw
            let prev = inp
                .prev_weights
                .as_ref()
                .and_then(|v| v.iter().find(|w| w.id == *id))
                .map(|w| w.weight);
            let smoothed = match prev {
                Some(p) => cfg.smooth_alpha * *raw + (1.0 - cfg.smooth_alpha) * p,
                None => *raw,
            };
            weights.push(PathWeight { id: *id, weight: smoothed });
        }

        // 最小份额并二次归一
        if cfg.min_share > 0.0 {
            let mut sum_after = 0.0;
            for w in weights.iter_mut() {
                w.weight = w.weight.max(cfg.min_share);
                sum_after += w.weight;
            }
            for w in weights.iter_mut() { w.weight /= sum_after.max(1e-9); }
        } else {
            let s: f64 = weights.iter().map(|w| w.weight).sum();
            if s > 0.0 { for w in weights.iter_mut() { w.weight /= s; } }
        }

        // Phase B - 输出排序信息（沿用原始日志风格，使用权重代替分数）
        let mut sorted_preview = weights.clone();
        sorted_preview.sort_by(|a, b| b.weight.total_cmp(&a.weight));
        tracing::debug!("Phase B - Sorted by weight:");
        for (i, w) in sorted_preview.iter().enumerate() {
            let eta = raws.iter().find(|(id,_,_)| *id == w.id).map(|(_,_,e)| *e).unwrap_or(0.0);
            let srtt = candidates.iter().find(|c| c.id == w.id).map(|c| c.srtt_ms).unwrap_or(0.0);
            tracing::debug!(
                "  #{} Path {}: weight={:.4}, eta={:.2}ms, srtt={:.2}ms",
                i + 1, w.id, w.weight, eta, srtt
            );
        }

        // Phase E: 冗余计划（top-2 by ETA）
        let mut eta_sorted = raws.clone();
        eta_sorted.sort_by(|a, b| a.2.total_cmp(&b.2));
        let (p1, e1) = (eta_sorted[0].0, eta_sorted[0].2);
        let (p2, e2) = (eta_sorted[1].0, eta_sorted[1].2);
        let eta_gap = e2 - e1;
        let w1 = weights.iter().find(|w| w.id == p1).map(|w| w.weight).unwrap_or(0.0);
        let w2 = weights.iter().find(|w| w.id == p2).map(|w| w.weight).unwrap_or(0.0);

        let redundancy = if eta_gap > delta_ms && w1 >= cfg.redundancy_min_weight && w2 >= cfg.redundancy_min_weight {
            tracing::debug!(
                "Phase E - Redundancy enabled: pair=({}, {}), eta_gap={:.2}ms > delta={:.2}ms, w1={:.3}, w2={:.3}",
                p1, p2, eta_gap, delta_ms, w1, w2
            );
            RedundancyMode::ShortIdleCritical {
                duplicate_first_n: 1,
                pair: (p1, p2),
                budget_pkts: cfg.redundancy_epoch_budget_pkts,
            }
        } else {
            tracing::debug!(
                "Phase E - Redundancy not triggered: eta_gap={:.2}ms, delta={:.2}ms, w1={:.3}, w2={:.3}",
                eta_gap, delta_ms, w1, w2
            );
            RedundancyMode::Off
        };

        tracing::debug!(
            "Final plan: valid_until_ms={}, weights_count={}, redundancy_mode={}",
            inp.now_ms + inp.epoch_ms as u64,
            weights.len(),
            match &redundancy { RedundancyMode::Off => "Off", RedundancyMode::ShortIdleCritical {..} => "ShortIdleCritical", RedundancyMode::Percentage {..} => "Percentage" }
        );
        
        Some(EpochPlan {
            valid_until_ms: inp.now_ms + inp.epoch_ms as u64,
            lb_weights: weights,
            redundancy,
        })
    }

    /// 兼容层：按包接口（用于旧调用方与外部用例）
    /// 内部借助 compute_plan 计算权重与冗余对，再结合旧触发条件输出 Decision
    pub fn pick_path(&self, inp: &PickInput) -> Option<Decision> {
        // 构造一个最小 epoch 输入（200ms 默认）
        let epoch_in = EpochInput {
            now_ms: inp.now_ms,
            epoch_ms: 200,
            packet_len_hint: inp.packet_len,
            paths: inp.paths.clone(),
            prev_weights: None,
        };
        let plan = self.compute_plan(&epoch_in)?;
        if plan.lb_weights.is_empty() {
            return None;
        }

        // 主路径：取权重最大的路径
        let primary = plan
            .lb_weights
            .iter()
            .max_by(|a, b| a.weight.partial_cmp(&b.weight).unwrap_or(Ordering::Equal))
            .map(|w| w.id)?;

        // 旧触发条件：短流/空闲/关键包
        let cfg = self.get_config();
        let is_short_flow = inp.flow_bytes_sent <= cfg.short_flow_thresh;
        let is_idle_burst = inp
            .paths
            .iter()
            .any(|p| p.last_tx_age_ms >= cfg.idle_burst_ms);
        let trigger = is_short_flow || is_idle_burst || inp.critical;

        // 根据 compute_plan 的冗余建议与触发条件决定是否复制
        let redundant = if trigger {
            match plan.redundancy {
                RedundancyMode::ShortIdleCritical { pair: (a, b), .. } |
                RedundancyMode::Percentage { pair: (a, b), .. } => {
                    // 从 pair 中挑选非 primary 的作为冗余目标
                    if a != primary && b != primary {
                        // 若 primary 不在 pair 中，选择权重更高的那个
                        let wa = plan.lb_weights.iter().find(|w| w.id == a).map(|w| w.weight).unwrap_or(0.0);
                        let wb = plan.lb_weights.iter().find(|w| w.id == b).map(|w| w.weight).unwrap_or(0.0);
                        Some(if wa >= wb { a } else { b })
                    } else if a == primary && b != primary {
                        Some(b)
                    } else if b == primary && a != primary {
                        Some(a)
                    } else {
                        None
                    }
                }
                RedundancyMode::Off => None,
            }
        } else {
            None
        };

        Some(Decision { primary, redundant })
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

        // Phase B: 在线打分（保留但不再用于最终决策，仅日志对齐）
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

    /// 应用 BLEST 乱序预算过滤（旧逻辑，保留以便回溯；权重化版本在 compute_plan 中实现）
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

    /// 应用 QAware 目标排队时延惩罚（旧逻辑，保留以便回溯；权重化版本在 compute_plan 中实现）
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

    /// 决定是否启用冗余发送（旧逻辑，保留以便回溯；权重化版本在 compute_plan 中输出路径对）
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

    fn weight_of(plan: &EpochPlan, id: PathId) -> f64 {
        plan.lb_weights.iter().find(|w| w.id == id).map(|w| w.weight).unwrap_or(0.0)
    }

    fn sum_weights(plan: &EpochPlan) -> f64 { plan.lb_weights.iter().map(|w| w.weight).sum() }

    fn approx(a: f64, b: f64, eps: f64) -> bool { (a - b).abs() <= eps }

    // 原逐包单元测试保留但不再适用 compute_plan，保留以便回溯/对比
    #[test]
    fn test_phase_a_filter_inactive() {
        let scheduler = Scheduler::new(Config::default());
        
        let mut path1 = create_test_path(1, 50.0, 1.0, 0.0);
        path1.active = false;
        let path2 = create_test_path(2, 100.0, 0.5, 0.0);

        let input = EpochInput {
            now_ms: 0,
            epoch_ms: 200,
            packet_len_hint: 1500,
            paths: vec![path1, path2],
            prev_weights: None,
        };

        let plan = scheduler.compute_plan(&input).unwrap();
        assert_eq!(plan.lb_weights.len(), 1, "应该只有活跃路径");
        assert_eq!(plan.lb_weights[0].id, 2, "应该选择活跃路径");
        assert!(matches!(plan.redundancy, RedundancyMode::Off));
    }

    #[test]
    fn test_phase_b_bandwidth_preference() {
        let scheduler = Scheduler::new(Config::default());
        let path1 = create_test_path(1, 50.0, 10.0, 0.0); // 高带宽
        let path2 = create_test_path(2, 50.0, 1.0, 0.0);  // 低带宽
        let input = EpochInput { now_ms: 0, epoch_ms: 200, packet_len_hint: 1500, paths: vec![path1, path2], prev_weights: None };
        let plan = scheduler.compute_plan(&input).unwrap();
        let w1 = weight_of(&plan, 1);
        let w2 = weight_of(&plan, 2);
        assert!(w1 > w2, "高带宽路径应获得更大权重: w1={} w2={}", w1, w2);
        assert!(w1 > 0.6, "高带宽路径权重应显著更高");
        assert!(approx(sum_weights(&plan), 1.0, 1e-6));
    }

    #[test]
    fn test_phase_b_loss_penalty() {
        let scheduler = Scheduler::new(Config::default());
        let path1 = create_test_path(1, 50.0, 5.0, 0.10); // 高丢包
        let path2 = create_test_path(2, 50.0, 5.0, 0.00); // 无丢包
        let input = EpochInput { now_ms: 0, epoch_ms: 200, packet_len_hint: 1500, paths: vec![path1, path2], prev_weights: None };
        let plan = scheduler.compute_plan(&input).unwrap();
        assert!(weight_of(&plan, 2) > weight_of(&plan, 1), "应避免高丢包路径");
    }

    #[test]
    fn test_phase_c_blest_gating() {
        let mut cfg = Config::default();
        cfg.enable_blest = true;
        let scheduler = Scheduler::new(cfg);
        let path1 = create_test_path(1, 50.0, 1.0, 0.0);   // 快路
        let path2 = create_test_path(2, 200.0, 1.0, 0.0);  // 慢路（ETA 远大于 fast+Δ）
        let input = EpochInput { now_ms: 0, epoch_ms: 200, packet_len_hint: 1500, paths: vec![path1, path2], prev_weights: None };
        let plan = scheduler.compute_plan(&input).unwrap();
        let w_fast = weight_of(&plan, 1);
        let w_slow = weight_of(&plan, 2);
        assert!(w_fast > w_slow, "BLEST 门控下慢路权重应被衰减");
        assert!(w_slow < 0.2, "慢路权重应足够小，避免乱序放大 (w_slow={})", w_slow);
    }

    #[test]
    fn test_phase_d_qaware_penalty() {
        let mut cfg = Config::default();
        cfg.enable_qaware = true;
        cfg.target_qdelay_ms = 10;
        let scheduler = Scheduler::new(cfg);
        let mut path1 = create_test_path(1, 50.0, 1.0, 0.0);
        path1.inflight_bytes = 100_000; // 高在途->高排队时延
        let path2 = create_test_path(2, 50.0, 1.0, 0.0);
        let input = EpochInput { now_ms: 0, epoch_ms: 200, packet_len_hint: 1500, paths: vec![path1, path2], prev_weights: None };
        let plan = scheduler.compute_plan(&input).unwrap();
        assert!(weight_of(&plan, 2) > weight_of(&plan, 1), "应避开高队列时延路径");
    }

    #[test]
    fn test_phase_e_redundancy_pair_by_eta() {
        let cfg = Config::default();
        let scheduler = Scheduler::new(cfg.clone());
        let path1 = create_test_path(1, 50.0, 1.0, 0.0);
        let path2 = create_test_path(2, 100.0, 1.0, 0.0);
        let input = EpochInput { now_ms: 0, epoch_ms: 200, packet_len_hint: 1500, paths: vec![path1.clone(), path2.clone()], prev_weights: None };
        let plan = scheduler.compute_plan(&input).unwrap();
        match plan.redundancy {
            RedundancyMode::ShortIdleCritical { pair, budget_pkts, .. } => {
                // pair 应为 ETA 最小的前两条（顺序不强制）
                let ids = [pair.0, pair.1];
                assert!(ids.contains(&1) && ids.contains(&2), "冗余路径应覆盖前两条路径");
                assert_eq!(budget_pkts, cfg.redundancy_epoch_budget_pkts);
            }
            RedundancyMode::Off => {
                // 允许由于权重门槛或 Δ 未满足而关闭，但此用例预期开启
                panic!("预期冗余开启，但为 Off");
            }
            _ => {}
        }
    }

    #[test]
    fn test_no_paths_available() {
        let scheduler = Scheduler::new(Config::default());
        let input = EpochInput { now_ms: 0, epoch_ms: 200, packet_len_hint: 1500, paths: vec![], prev_weights: None };
        let plan = scheduler.compute_plan(&input);
        assert!(plan.is_none(), "没有路径应返回 None");
    }

    #[test]
    fn test_single_path() {
        let scheduler = Scheduler::new(Config::default());
        let path1 = create_test_path(1, 50.0, 1.0, 0.0);
        let input = EpochInput { now_ms: 0, epoch_ms: 200, packet_len_hint: 1500, paths: vec![path1], prev_weights: None };
        let plan = scheduler.compute_plan(&input).unwrap();
        assert_eq!(plan.lb_weights.len(), 1);
        assert_eq!(plan.lb_weights[0].id, 1);
        assert!(approx(plan.lb_weights[0].weight, 1.0, 1e-9));
        assert!(matches!(plan.redundancy, RedundancyMode::Off));
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
        let scheduler = Scheduler::new(Config::default());
        let path1 = create_test_path(1, 50.0, 10.0, 0.05);  // 快路,低丢包
        let path2 = create_test_path(2, 250.0, 5.0, 0.10);  // 慢路,高丢包
        let input = EpochInput { now_ms: 0, epoch_ms: 200, packet_len_hint: 1500, paths: vec![path1, path2], prev_weights: None };
        let plan = scheduler.compute_plan(&input).unwrap();
        assert!(weight_of(&plan, 1) > weight_of(&plan, 2), "应该选择快速低丢包路径");
    }

    #[test]
    fn test_realistic_scenario_50ms_5pct_loss() {
        let scheduler = Scheduler::new(Config::default());
        let path1 = create_test_path(1, 50.0, 5.0, 0.05);
        let path2 = create_test_path(2, 50.0, 5.0, 0.05);
        let input = EpochInput { now_ms: 0, epoch_ms: 200, packet_len_hint: 1500, paths: vec![path1, path2], prev_weights: None };
        let plan = scheduler.compute_plan(&input).unwrap();
        let w1 = weight_of(&plan, 1);
        let w2 = weight_of(&plan, 2);
        assert!(w1 > 0.3 && w2 > 0.3, "两路应较为均衡");
        assert!((w1 - w2).abs() < 0.25, "两路权重差异不应过大: w1={} w2={}", w1, w2);
    }

    #[test]
    fn test_ema_smoothing() {
        let mut cfg = Config::default();
        cfg.smooth_alpha = 0.6;
        let scheduler = Scheduler::new(cfg);
        // 原权重偏向路径1
        let prev = vec![PathWeight { id: 1, weight: 1.0 }, PathWeight { id: 2, weight: 0.0 }];
        // 当前原始偏向路径2（通过带宽差异实现）
        let p1 = create_test_path(1, 50.0, 1.0, 0.0);
        let p2 = create_test_path(2, 50.0, 100.0, 0.0);
        let input = EpochInput { now_ms: 0, epoch_ms: 200, packet_len_hint: 1500, paths: vec![p1, p2], prev_weights: Some(prev) };
        let plan = scheduler.compute_plan(&input).unwrap();
        let w1 = weight_of(&plan, 1);
        let w2 = weight_of(&plan, 2);
        // 平滑后路径2占优，但不会接近1.0
        assert!(w2 > w1, "EMA 后应逐步向新的原始偏好移动");
        assert!(w2 < 0.9, "EMA 抑制瞬时抖动，权重不应过于极端");
    }

    #[test]
    fn test_min_share_floor() {
        let mut cfg = Config::default();
        cfg.min_share = 0.05; // 启用最小份额
        let scheduler = Scheduler::new(cfg);
        let p1 = create_test_path(1, 50.0, 10.0, 0.0);
        let p2 = create_test_path(2, 50.0, 1.0, 0.0);  // 弱路但不极端
        let input = EpochInput { now_ms: 0, epoch_ms: 200, packet_len_hint: 1500, paths: vec![p1, p2], prev_weights: None };
        let plan = scheduler.compute_plan(&input).unwrap();
        for w in &plan.lb_weights {
            assert!(w.weight >= 0.05 - 1e-6, "所有活跃路径应不低于最小份额: actual={:.6}", w.weight);
        }
        assert!(approx(sum_weights(&plan), 1.0, 1e-6));
    }

    #[test]
    fn test_redundancy_weight_gate() {
        let mut cfg = Config::default();
        cfg.redundancy_min_weight = 0.5; // 提高门槛
        let scheduler = Scheduler::new(cfg);
        // 路径1权重大，路径2很小，尽管 ETA gap > Δ 也不应触发冗余
        let p1 = create_test_path(1, 50.0, 10.0, 0.0);
        let p2 = create_test_path(2, 100.0, 1.0, 0.0);
        let input = EpochInput { now_ms: 0, epoch_ms: 200, packet_len_hint: 1500, paths: vec![p1, p2], prev_weights: None };
        let plan = scheduler.compute_plan(&input).unwrap();
        assert!(matches!(plan.redundancy, RedundancyMode::Off), "权重不足应不触发冗余");
    }
}
