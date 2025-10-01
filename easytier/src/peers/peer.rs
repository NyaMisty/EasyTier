use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::{DashMap, DashSet};

use tokio::{select, sync::mpsc};

use tracing::Instrument;

use super::{
    peer_conn::{PeerConn, PeerConnId},
    PacketRecvChan,
    multipath_scheduler::{
        Scheduler as MultipathScheduler,
        Config as SchedulerConfig,
        EpochInput,
        EpochPlan,
        RedundancyMode,
        PathSnapshot,
        PathWeight,
    },
};
use crate::{common::scoped_task::ScopedTask, proto::cli::PeerConnInfo};
use crate::{
    common::{
        error::Error,
        global_ctx::{ArcGlobalCtx, GlobalCtxEvent},
        PeerId,
    },
    tunnel::packet_def::ZCPacket,
};

// 新增：加权采样仅用 RNG，索引在计划刷新时预计算
use rand::Rng;

type ArcPeerConn = Arc<PeerConn>;
type ConnMap = Arc<DashMap<PeerConnId, ArcPeerConn>>;

// 预计算的负载均衡选择器（累计权重表）
#[derive(Clone, Debug, Default)]
struct LbSelector {
    // entries 中的第二个值是累计权重，严格递增，最后一个应为 1.0
    entries: Vec<(PeerConnId, f64)>,
}

// 冗余状态（按 epoch 预算）
#[derive(Clone, Debug, Default)]
struct RedundancyState {
    valid_until_ms: u64,
    pair: Option<(PeerConnId, PeerConnId)>,
    remaining_pkts: u32,
}

pub struct Peer {
    pub peer_node_id: PeerId,
    conns: ConnMap,
    global_ctx: ArcGlobalCtx,

    packet_recv_chan: PacketRecvChan,

    close_event_sender: mpsc::Sender<PeerConnId>,
    close_event_listener: ScopedTask<()>,

    shutdown_notifier: Arc<tokio::sync::Notify>,

    // 多路径调度器相关
    multipath_scheduler: Arc<MultipathScheduler>,
    // 旧：默认连接缓存（保留给 get_default_conn_id 等）
    default_conn_ids: Arc<ArcSwap<Vec<PeerConnId>>>,
    // 新：epoch 计划缓存与平滑权重
    epoch_plan: Arc<ArcSwap<Option<EpochPlan>>>,
    prev_weights: Arc<ArcSwap<Vec<PathWeight>>>,
    // 新：预计算选择器（仅在计划刷新时重建）
    lb_selector: Arc<ArcSwap<Option<LbSelector>>>,
    // 冗余预算状态
    redundancy_state: Arc<tokio::sync::Mutex<RedundancyState>>,


    select_update_mutex: Arc<tokio::sync::Mutex<()>>,
    select_update_notify: Arc<tokio::sync::Notify>,
    default_conn_refresh_task: ScopedTask<()>,
}

impl Peer {
    pub fn new(
        peer_node_id: PeerId,
        packet_recv_chan: PacketRecvChan,
        global_ctx: ArcGlobalCtx,
    ) -> Self {
        let conns: ConnMap = Arc::new(DashMap::new());
        let (close_event_sender, mut close_event_receiver) = mpsc::channel(10);
        let shutdown_notifier = Arc::new(tokio::sync::Notify::new());

        // 初始化新的并发控制与周期刷新机制（提前创建，供后续监听器使用）
        let default_conn_ids = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let epoch_plan = Arc::new(ArcSwap::from_pointee(None::<EpochPlan>));
        let prev_weights = Arc::new(ArcSwap::from_pointee(Vec::<PathWeight>::new()));
        let lb_selector = Arc::new(ArcSwap::from_pointee(None::<LbSelector>));
        let redundancy_state = Arc::new(tokio::sync::Mutex::new(RedundancyState::default()));
        let select_update_mutex = Arc::new(tokio::sync::Mutex::new(()));
        let select_update_notify = Arc::new(tokio::sync::Notify::new());

        // 关闭事件监听器（在移除连接时触发一次刷新）
        let conns_copy = conns.clone();
        let shutdown_notifier_copy = shutdown_notifier.clone();
        let global_ctx_copy = global_ctx.clone();
        let default_conn_ids_copy_for_close = default_conn_ids.clone();
        let epoch_plan_copy_for_close = epoch_plan.clone();
        let prev_weights_copy_for_close = prev_weights.clone();
        let lb_selector_copy_for_close = lb_selector.clone();
        let redundancy_state_copy_for_close = redundancy_state.clone();
        let select_update_mutex_copy_for_close = select_update_mutex.clone();
        let select_update_notify_copy_for_close = select_update_notify.clone();
        let scheduler_copy_for_close = Arc::new(MultipathScheduler::new(SchedulerConfig::default()));
        let close_event_listener = tokio::spawn(
            async move {
                loop {
                    select! {
                        ret = close_event_receiver.recv() => {
                            if ret.is_none() {
                                break;
                            }
                            let ret = ret.unwrap();
                            tracing::warn!(
                                ?peer_node_id,
                                ?ret,
                                "notified that peer conn is closed",
                            );

                            if let Some((_, conn)) = conns_copy.remove(&ret) {
                                global_ctx_copy.issue_event(GlobalCtxEvent::PeerConnRemoved(
                                    conn.get_conn_info(),
                                ));
                                // 触发一次立即刷新（不阻塞监听器）
                                let conns_for_task = conns_copy.clone();
                                let default_ids_for_task = default_conn_ids_copy_for_close.clone();
                                let epoch_plan_for_task = epoch_plan_copy_for_close.clone();
                                let prev_weights_for_task = prev_weights_copy_for_close.clone();
                                let lb_selector_for_task = lb_selector_copy_for_close.clone();
                                let redundancy_for_task = redundancy_state_copy_for_close.clone();
                                let mutex_for_task = select_update_mutex_copy_for_close.clone();
                                let notify_for_task = select_update_notify_copy_for_close.clone();
                                let scheduler_for_task = scheduler_copy_for_close.clone();
                                tokio::spawn(async move {
                                    Peer::refresh_epoch_plan_with_delay(
                                        peer_node_id,
                                        conns_for_task,
                                        scheduler_for_task,
                                        prev_weights_for_task,
                                        epoch_plan_for_task,
                                        default_ids_for_task,
                                        lb_selector_for_task,
                                        redundancy_for_task,
                                        mutex_for_task,
                                        notify_for_task,
                                        std::time::Duration::from_millis(0),
                                    ).await;
                                });
                            }
                        }

                        _ = shutdown_notifier_copy.notified() => {
                            close_event_receiver.close();
                            tracing::warn!(?peer_node_id, "peer close event listener notified");
                        }
                    }
                }
                tracing::info!("peer {} close event listener exit", peer_node_id);
            }
            .instrument(tracing::info_span!(
                "peer_close_event_listener",
                ?peer_node_id,
            )),
        )
        .into();

        // 周期刷新 epoch plan，每 200ms 执行一次
        let conns_copy = conns.clone();
        let default_conn_ids_copy = default_conn_ids.clone();
        let epoch_plan_copy = epoch_plan.clone();
        let prev_weights_copy = prev_weights.clone();
        let lb_selector_copy = lb_selector.clone();
        let redundancy_state_copy = redundancy_state.clone();
        let select_update_mutex_copy = select_update_mutex.clone();
        let select_update_notify_copy = select_update_notify.clone();
        let shutdown_notifier_copy = shutdown_notifier.clone();
        let peer_node_id_copy = peer_node_id;
        let scheduler_copy = Arc::new(MultipathScheduler::new(SchedulerConfig::default()));
        let default_conn_refresh_task = ScopedTask::from(tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
            loop {
                select! {
                    _ = shutdown_notifier_copy.notified() => {
                        tracing::info!(?peer_node_id_copy, "epoch plan refresher shutdown");
                        break;
                    }
                    _ = interval.tick() => {
                        Peer::refresh_epoch_plan_with_delay(
                            peer_node_id_copy,
                            conns_copy.clone(),
                            scheduler_copy.clone(),
                            prev_weights_copy.clone(),
                            epoch_plan_copy.clone(),
                            default_conn_ids_copy.clone(),
                            lb_selector_copy.clone(),
                            redundancy_state_copy.clone(),
                            select_update_mutex_copy.clone(),
                            select_update_notify_copy.clone(),
                            std::time::Duration::from_millis(0),
                        ).await;
                    }
                }
            }
        }));

        // 创建多路径调度器（实例成员，供其他场景复用）
        let scheduler_config = SchedulerConfig::default();
        let multipath_scheduler = Arc::new(MultipathScheduler::new(scheduler_config));

        Peer {
            peer_node_id,
            conns: conns.clone(),
            packet_recv_chan,
            global_ctx,

            close_event_sender,
            close_event_listener,

            shutdown_notifier,
            multipath_scheduler,
            default_conn_ids,
            epoch_plan,
            prev_weights,
            lb_selector,
            redundancy_state,
            select_update_mutex,
            select_update_notify,
            default_conn_refresh_task,
        }
    }

    fn refresh_epoch_plan_with_delay_func(
        &self,
        delay: std::time::Duration,
    ) -> impl std::future::Future<Output = bool> {
        let peer_node_id_copy = self.peer_node_id;
        let conns_copy = self.conns.clone();
        let scheduler_copy = self.multipath_scheduler.clone();
        let prev_weights_copy = self.prev_weights.clone();
        let epoch_plan_copy = self.epoch_plan.clone();
        let default_conn_ids_copy = self.default_conn_ids.clone();
        let lb_selector_copy = self.lb_selector.clone();
        let redundancy_state_copy = self.redundancy_state.clone();
        let select_update_mutex_copy = self.select_update_mutex.clone();
        let select_update_notify_copy = self.select_update_notify.clone();

        async move {
            Peer::refresh_epoch_plan_with_delay(
                peer_node_id_copy,
                conns_copy,
                scheduler_copy,
                prev_weights_copy,
                epoch_plan_copy,
                default_conn_ids_copy,
                lb_selector_copy,
                redundancy_state_copy,
                select_update_mutex_copy,
                select_update_notify_copy,
                delay,
            ).await
        }
    }

    // 提供一个可复用的刷新函数，允许在刷新前 sleep 指定时长
    async fn refresh_epoch_plan_with_delay(
        peer_node_id: PeerId,
        conns: ConnMap,
        scheduler: Arc<MultipathScheduler>,
        prev_weights: Arc<ArcSwap<Vec<PathWeight>>>,

        epoch_plan_store: Arc<ArcSwap<Option<EpochPlan>>>,
        default_conn_ids: Arc<ArcSwap<Vec<PeerConnId>>>,
        lb_selector_store: Arc<ArcSwap<Option<LbSelector>>>,

        redundancy_state: Arc<tokio::sync::Mutex<RedundancyState>>,
        select_update_mutex: Arc<tokio::sync::Mutex<()>>,
        select_update_notify: Arc<tokio::sync::Notify>,
        delay: std::time::Duration,
    ) -> bool {
        if delay.as_millis() > 0 {
            tokio::time::sleep(delay).await;
        }
        if let Ok(_guard) = select_update_mutex.try_lock() {
            // 构建 PathSnapshot
            let mut path_snapshots: Vec<(PeerConnId, PathSnapshot)> = Vec::new();
            for conn in conns.iter() {
                let conn_ref = conn.value();
                let conn_id = conn.get_conn_id();
                let tunnel_metrics = None;
                let snapshot = conn_ref.get_path_snapshot(conn_id.as_u128() as u64, tunnel_metrics);
                path_snapshots.push((conn_id, snapshot));
            }

            // 生成 EpochInput
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let prev = {
                let v = prev_weights.load();
                if v.is_empty() { None } else { Some(v.as_ref().clone()) }
            };
            let input = EpochInput {
                now_ms,
                epoch_ms: 200,
                packet_len_hint: 1500,
                paths: path_snapshots.iter().map(|(_, s)| s.clone()).collect(),
                prev_weights: prev,
            };

            // 计算计划
            let plan_opt = scheduler.compute_plan(&input);
            // 存储并广播
            match plan_opt {
                Some(plan) => {
                    // 日志输出最终的 epoch plan（有效期、各路径权重、冗余配置）
                    tracing::info!(
                        ?peer_node_id,
                        valid_until_ms = plan.valid_until_ms,
                        weights_count = plan.lb_weights.len(),
                        "epoch plan finalized"
                    );
                    for w in &plan.lb_weights {
                        if let Some((peer_id, _)) = path_snapshots.iter().find(|(_, s)| s.id == w.id) {
                            tracing::info!(
                                ?peer_node_id,
                                conn_id = ?peer_id,
                                path_id = w.id,
                                weight = w.weight,
                                "  weight entry"
                            );
                        } else {
                            tracing::info!(
                                ?peer_node_id,
                                path_id = w.id,
                                weight = w.weight,
                                "  weight entry (no conn mapping)"
                            );
                        }
                    }
                    match &plan.redundancy {
                        RedundancyMode::Off => {
                            tracing::info!(?peer_node_id, "  redundancy: Off");
                        }
                        RedundancyMode::ShortIdleCritical { pair: (a, b), budget_pkts, duplicate_first_n } => {
                            let map = |pid: u64| -> Option<PeerConnId> {
                                path_snapshots.iter().find(|(_, s)| s.id == pid).map(|(id, _)| *id)
                            };
                            let aid = map(*a);
                            let bid = map(*b);
                            tracing::info!(
                                ?peer_node_id,
                                pair_a = *a,
                                pair_b = *b,
                                pair_conn_a = ?aid,
                                pair_conn_b = ?bid,
                                budget_pkts = *budget_pkts,
                                duplicate_first_n = *duplicate_first_n,
                                "  redundancy: ShortIdleCritical"
                            );
                        }
                        RedundancyMode::Percentage { pair: (a, b), budget_pkts, percent } => {
                            let map = |pid: u64| -> Option<PeerConnId> {
                                path_snapshots.iter().find(|(_, s)| s.id == pid).map(|(id, _)| *id)
                            };
                            let aid = map(*a);
                            let bid = map(*b);
                            tracing::info!(
                                ?peer_node_id,
                                pair_a = *a,
                                pair_b = *b,
                                pair_conn_a = ?aid,
                                pair_conn_b = ?bid,
                                budget_pkts = *budget_pkts,
                                percent = *percent as f64,
                                "  redundancy: Percentage"
                            );
                        }
                    }

                    // 保存完整计划
                    epoch_plan_store.store(Arc::new(Some(plan.clone())));
                    // 更新 prev_weights
                    prev_weights.store(Arc::new(plan.lb_weights.clone()));

                    // 更新默认 conn ids（取权重最大为 primary，若冗余则附加对应 pair 中另一路）
                    let mut ids: Vec<PeerConnId> = Vec::new();
                    if !plan.lb_weights.is_empty() {
                        // 找到权重最大
                        if let Some(max_w) = plan.lb_weights.iter().max_by(|a, b| a.weight.total_cmp(&b.weight)) {
                            let primary_id_u64 = max_w.id;
                            if let Some((peer_id, _)) = path_snapshots.iter().find(|(_, s)| s.id == primary_id_u64) {
                                ids.push(*peer_id);
                            }
                        }
                        // 冗余：若 primary 在 pair 中，追加另外一路
                        if let RedundancyMode::ShortIdleCritical { pair: (a, b), .. } | RedundancyMode::Percentage { pair: (a, b), .. } = &plan.redundancy {
                            // 将 pair 转换为 PeerConnId
                            let map = |pid: u64| -> Option<PeerConnId> {
                                path_snapshots.iter().find(|(_, s)| s.id == pid).map(|(id, _)| *id)
                            };
                            if let (Some(aid), Some(bid)) = (map(*a), map(*b)) {
                                if let Some(primary) = ids.first() {
                                    if *primary == aid { ids.push(bid); }
                                    else if *primary == bid { ids.push(aid); }
                                }
                            }
                        }
                    }
                    default_conn_ids.store(Arc::new(ids));

                    // 预计算并缓存加权选择器（累计权重）
                    let mut selector_entries: Vec<(PeerConnId, f64)> = Vec::new();
                    let mut sum = 0.0_f64;
                    for w in &plan.lb_weights {
                        let weight = w.weight.max(0.0);
                        if weight <= 0.0 { continue; }
                        if let Some((peer_id, _)) = path_snapshots.iter().find(|(_, s)| s.id == w.id) {
                            selector_entries.push((*peer_id, weight));
                            sum += weight;
                        }
                    }
                    if sum > 0.0 && !selector_entries.is_empty() {
                        // 归一化并转累计
                        let mut cum = 0.0_f64;
                        for (_id, wt) in selector_entries.iter_mut() {
                            *wt /= sum;
                            cum += *wt;
                            *wt = cum;
                        }
                        // 防止浮点误差导致最后一项<1
                        if let Some((_, last)) = selector_entries.last_mut() { *last = 1.0; }
                        lb_selector_store.store(Arc::new(Some(LbSelector { entries: selector_entries })));
                    } else {
                        lb_selector_store.store(Arc::new(None));
                    }

                    // 更新冗余预算
                    let mut st = redundancy_state.lock().await;
                    match &plan.redundancy {
                        RedundancyMode::Off => {
                            st.valid_until_ms = plan.valid_until_ms;
                            st.pair = None;
                            st.remaining_pkts = 0;
                        }
                        RedundancyMode::ShortIdleCritical { pair: (a, b), budget_pkts, .. } |
                        RedundancyMode::Percentage { pair: (a, b), budget_pkts, .. } => {
                            let map = |pid: u64| -> Option<PeerConnId> {
                                path_snapshots.iter().find(|(_, s)| s.id == pid).map(|(id, _)| *id)
                            };
                            let pair_opt = match (map(*a), map(*b)) { (Some(x), Some(y)) => Some((x, y)), _ => None };
                            st.valid_until_ms = plan.valid_until_ms;
                            st.pair = pair_opt;
                            st.remaining_pkts = *budget_pkts;
                        }
                    }

                    // 通知可能等待 select_conns 的协程
                    select_update_notify.notify_waiters();
                    return true;
                }
                None => {
                    tracing::info!(?peer_node_id, "epoch plan is None (no usable paths)");
                    // 没有可用路径
                    epoch_plan_store.store(Arc::new(None));
                    default_conn_ids.store(Arc::new(Vec::new()));
                    lb_selector_store.store(Arc::new(None));
                    let mut st = redundancy_state.lock().await;
                    st.valid_until_ms = now_ms + 200;
                    st.pair = None;
                    st.remaining_pkts = 0;
                    select_update_notify.notify_waiters();
                    return true;
                }
            }
        }
        false
    }

    pub async fn add_peer_conn(&self, mut conn: PeerConn) {
        let close_notifier = conn.get_close_notifier();
        let conn_info = conn.get_conn_info();

        conn.start_recv_loop(self.packet_recv_chan.clone()).await;
        conn.start_pingpong();
        self.conns.insert(conn.get_conn_id(), Arc::new(conn));

        let close_event_sender = self.close_event_sender.clone();
        tokio::spawn(async move {
            let conn_id = close_notifier.get_conn_id();
            if let Some(mut waiter) = close_notifier.get_waiter().await {
                let _ = waiter.recv().await;
            }
            if let Err(e) = close_event_sender.send(conn_id).await {
                tracing::warn!(?conn_id, "failed to send close event: {}", e);
            }
        });

        self.global_ctx
            .issue_event(GlobalCtxEvent::PeerConnAdded(conn_info));

        // 立即触发一次刷新（不阻塞调用方）
        self.refresh_epoch_plan_with_delay_func(std::time::Duration::from_millis(0)).await;
    }

    // const MAX_CONN_AGE_MS: u64 = 800; // 800ms
    const MAX_CONN_AGE_MS: u64 = 1_200; // 800ms
    // const MAX_CONN_AGE_MS: u64 = 1_500;           // 1.5s
    const TCP_PREFERENCE_DELTA_US: u64 = 50_000;  // 50ms (微秒)

    // 使用 multipath 调度器计算 epoch 计划（不再返回固定默认连接集合）
    async fn compute_default_conn_ids_from_conns(
        _peer_node_id: PeerId,
        _conns: &ConnMap,
        _prefer_tcp: bool,
    ) -> Vec<PeerConnId> {
        // 兼容旧接口：不再使用，返回空，由刷新任务填充 default_conn_ids
        vec![]
    }

    async fn select_conns(&self) -> Vec<ArcPeerConn> {
        // 优先使用计划；若没有则尝试等待一次刷新
        let mut plan_opt = self.epoch_plan.load();
        if plan_opt.is_none() {
            if !self.refresh_epoch_plan_with_delay_func(std::time::Duration::from_millis(0)).await {
                loop {
                    self.select_update_notify.notified().await;
                    let p = self.epoch_plan.load();
                    if p.is_some() || self.conns.is_empty() { break; }
                }
            }
            plan_opt = self.epoch_plan.load();
        }

        // 快速路径：有预计算选择器
        let selector_opt = self.lb_selector.load();
        let primary_id_opt = if let Some(sel) = selector_opt.as_ref() {
            if sel.entries.is_empty() { None } else {
                let x: f64 = rand::thread_rng().gen(); // [0,1)
                // 线性扫描（路径数通常很小）。若需更优可二分。
                let mut picked: Option<PeerConnId> = None;
                for (cid, cum) in &sel.entries {
                    if x <= *cum { picked = Some(*cid); break; }
                }
                picked.or_else(|| sel.entries.last().map(|(id, _)| *id))
            }
        } else { None };

        // 如果没有 selector（如 plan 不可用），做等概率选择
        let primary_conn = if let Some(pid) = primary_id_opt {
            self.conns.get(&pid).map(|c| c.clone())
        } else {
            let count = self.conns.len();
            if count == 0 { None } else {
                let idx = rand::thread_rng().gen_range(0..count);
                self.conns.iter().nth(idx).map(|e| e.clone())
            }
        };

        let Some(primary_conn) = primary_conn else { return vec![]; };
        let primary_id = primary_conn.get_conn_id();

        // 判断是否需要冗余副本
        let mut result = vec![primary_conn];
        if let Some(_plan) = plan_opt.as_ref().clone() {
            // 检查预算与配对
            let mut need_redundant = false;
            let mut partner: Option<PeerConnId> = None;
            {
                let mut st = self.redundancy_state.lock().await;
                // 超期则不再使用旧预算
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                if now_ms > st.valid_until_ms { st.remaining_pkts = 0; st.pair = None; }
                if st.remaining_pkts > 0 {
                    if let Some((a, b)) = st.pair {
                        if primary_id == a { partner = Some(b); }
                        else if primary_id == b { partner = Some(a); }
                    }
                }
                if partner.is_some() { st.remaining_pkts = st.remaining_pkts.saturating_sub(1); need_redundant = true; }
            }

            if need_redundant {
                if let Some(pid) = partner {
                    if let Some(conn) = self.conns.get(&pid) {
                        result.push(conn.clone());
                    }
                }
            } else {
                // 如果预算为 0，但计划是 Percentage 模式，可按比例小概率复制（可选：这里保持不复制，遵循预算）
                let _ = _plan; // 保留变量，便于未来扩展
            }
        }

        result
    }

    pub async fn send_msg(&self, msg: ZCPacket) -> Result<(), Error> {
        let conns = self.select_conns().await;
        if conns.is_empty() {
            return Err(Error::PeerNoConnectionError(self.peer_node_id));
        };
        tracing::debug!(
            ?self.peer_node_id,
            conn_count = conns.len(),
            "sending msg to conns: {:?}", conns
        );
        if conns.len() == 1 {
            let conn = conns[0].clone();
            conn.send_msg(msg).await?;
            return Ok(());
        }
        // 并发发送，任意一个成功即返回，不等待其他 future（冗余多发包）
        use futures::StreamExt;
        let mut futs = futures::stream::FuturesUnordered::new();
        for conn in conns.iter() {
            let conn = conn.clone();
            let conn_id = conn.get_conn_id();
            let packet = msg.clone();
            futs.push(async move { (conn_id, conn.send_msg(packet).await) });
        }

        while let Some((conn_id, res)) = futs.next().await {
            match res {
                Ok(()) => {
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(?e, ?conn_id, "failed to send msg to peer");
                }
            }
        }

        // 全部失败
        Err(Error::PeerNoConnectionError(self.peer_node_id))
    }

    pub async fn close_peer_conn(&self, conn_id: &PeerConnId) -> Result<(), Error> {
        let has_key = self.conns.contains_key(conn_id);
        if !has_key {
            return Err(Error::NotFound);
        }
        self.close_event_sender.send(*conn_id).await.unwrap();
        Ok(())
    }

    pub async fn list_peer_conns(&self) -> Vec<PeerConnInfo> {
        let mut conns = vec![];
        for conn in self.conns.iter() {
            // do not lock here, otherwise it will cause dashmap deadlock
            conns.push(conn.clone());
        }

        let mut ret = Vec::new();
        for conn in conns {
            let info = conn.get_conn_info();
            if !info.is_closed {
                ret.push(info);
            } else {
                let conn_id = info.conn_id.parse().unwrap();
                let _ = self.close_peer_conn(&conn_id).await;
            }
        }
        ret
    }

    pub fn has_directly_connected_conn(&self) -> bool {
        self.conns
            .iter()
            .any(|entry| !(entry.value()).is_hole_punched())
    }

    pub fn get_directly_connections(&self) -> DashSet<uuid::Uuid> {
        self.conns
            .iter()
            .filter(|entry| !(entry.value()).is_hole_punched())
            .map(|entry| (entry.value()).get_conn_id())
            .collect()
    }

    pub fn get_default_conn_id(&self) -> PeerConnId {
        let conn_ids = self.default_conn_ids.load();
        if !conn_ids.is_empty() {
            conn_ids[0]
        } else {
            PeerConnId::default()
        }
    }
}

// pritn on drop
impl Drop for Peer {
    fn drop(&mut self) {
        self.shutdown_notifier.notify_one();
        tracing::info!("peer {} drop", self.peer_node_id);
    }
}

#[cfg(test)]
mod tests {

    use tokio::time::timeout;

    use crate::{
        common::{global_ctx::tests::get_mock_global_ctx, new_peer_id},
        peers::{create_packet_recv_chan, peer_conn::PeerConn},
        tunnel::ring::create_ring_tunnel_pair,
    };

    use super::Peer;

    #[tokio::test]
    async fn close_peer() {
        let (local_packet_send, _local_packet_recv) = create_packet_recv_chan();
        let (remote_packet_send, _remote_packet_recv) = create_packet_recv_chan();
        let global_ctx = get_mock_global_ctx();
        let local_peer = Peer::new(new_peer_id(), local_packet_send, global_ctx.clone());
        let remote_peer = Peer::new(new_peer_id(), remote_packet_send, global_ctx.clone());

        let (local_tunnel, remote_tunnel) = create_ring_tunnel_pair();
        let mut local_peer_conn =
            PeerConn::new(local_peer.peer_node_id, global_ctx.clone(), local_tunnel);
        let mut remote_peer_conn =
            PeerConn::new(remote_peer.peer_node_id, global_ctx.clone(), remote_tunnel);

        assert!(!local_peer_conn.handshake_done());
        assert!(!remote_peer_conn.handshake_done());

        let (a, b) = tokio::join!(
            local_peer_conn.do_handshake_as_client(),
            remote_peer_conn.do_handshake_as_server()
        );
        a.unwrap();
        b.unwrap();

        let local_conn_id = local_peer_conn.get_conn_id();

        local_peer.add_peer_conn(local_peer_conn).await;
        remote_peer.add_peer_conn(remote_peer_conn).await;

        assert_eq!(local_peer.list_peer_conns().await.len(), 1);
        assert_eq!(remote_peer.list_peer_conns().await.len(), 1);

        let close_handler =
            tokio::spawn(async move { local_peer.close_peer_conn(&local_conn_id).await });

        // wait for remote peer conn close
        timeout(std::time::Duration::from_secs(5), async {
            while !remote_peer.list_peer_conns().await.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();

        println!("wait for close handler");
        close_handler.await.unwrap().unwrap();
    }
}
