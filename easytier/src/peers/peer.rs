use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::{DashMap, DashSet};

use tokio::{select, sync::mpsc};

use tracing::Instrument;

use super::{
    peer_conn::{PeerConn, PeerConnId},
    PacketRecvChan,
    multipath_scheduler::{Scheduler as MultipathScheduler, Config as SchedulerConfig, PickInput, PathSnapshot},
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

type ArcPeerConn = Arc<PeerConn>;
type ConnMap = Arc<DashMap<PeerConnId, ArcPeerConn>>;

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
    default_conn_ids: Arc<ArcSwap<Vec<PeerConnId>>>,
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
        let select_update_mutex = Arc::new(tokio::sync::Mutex::new(()));
        let select_update_notify = Arc::new(tokio::sync::Notify::new());

        // 关闭事件监听器（在移除连接时触发一次刷新）
        let conns_copy = conns.clone();
        let shutdown_notifier_copy = shutdown_notifier.clone();
        let global_ctx_copy = global_ctx.clone();
        let default_conn_ids_copy_for_close = default_conn_ids.clone();
        let select_update_mutex_copy_for_close = select_update_mutex.clone();
        let select_update_notify_copy_for_close = select_update_notify.clone();
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
                                let mutex_for_task = select_update_mutex_copy_for_close.clone();
                                let notify_for_task = select_update_notify_copy_for_close.clone();
                                tokio::spawn(async move {
                                    Peer::refresh_default_conn_ids_with_delay(
                                        peer_node_id,
                                        conns_for_task,
                                        default_ids_for_task,
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

        // 周期刷新 default_conn_ids，每 200ms 执行一次
        let conns_copy = conns.clone();
        let default_conn_ids_copy = default_conn_ids.clone();
        let select_update_mutex_copy = select_update_mutex.clone();
        let select_update_notify_copy = select_update_notify.clone();
        let shutdown_notifier_copy = shutdown_notifier.clone();
        let peer_node_id_copy = peer_node_id;
        let default_conn_refresh_task = ScopedTask::from(tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
            loop {
                select! {
                    _ = shutdown_notifier_copy.notified() => {
                        tracing::info!(?peer_node_id_copy, "default_conn refresher shutdown");
                        break;
                    }
                    _ = interval.tick() => {
                        // 调用抽取的刷新函数（可配置起始延迟，这里为0）
                        Peer::refresh_default_conn_ids_with_delay(
                            peer_node_id_copy,
                            conns_copy.clone(),
                            default_conn_ids_copy.clone(),
                            select_update_mutex_copy.clone(),
                            select_update_notify_copy.clone(),
                            std::time::Duration::from_millis(0),
                        ).await;
                    }
                }
            }
        }));

        // 创建多路径调度器
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
            select_update_mutex,
            select_update_notify,
            default_conn_refresh_task,
        }
    }

    fn refresh_default_conn_ids_with_delay_func(
        &self,
        delay: std::time::Duration,
    ) -> impl std::future::Future<Output = bool> {
        let peer_node_id_copy = self.peer_node_id;
        let conns_copy = self.conns.clone();
        let default_conn_ids_copy = self.default_conn_ids.clone();
        let select_update_mutex_copy = self.select_update_mutex.clone();
        let select_update_notify_copy = self.select_update_notify.clone();

        async move {
            Peer::refresh_default_conn_ids_with_delay(
                peer_node_id_copy,
                conns_copy,
                default_conn_ids_copy,
                select_update_mutex_copy,
                select_update_notify_copy,
                delay,
            ).await
        }
    }

    // 提供一个可复用的刷新函数，允许在刷新前 sleep 指定时长
    async fn refresh_default_conn_ids_with_delay(
        peer_node_id: PeerId,
        conns: ConnMap,
        default_conn_ids: Arc<ArcSwap<Vec<PeerConnId>>>,
        select_update_mutex: Arc<tokio::sync::Mutex<()>>,
        select_update_notify: Arc<tokio::sync::Notify>,
        delay: std::time::Duration,
    ) -> bool {
        if delay.as_millis() > 0 {
            tokio::time::sleep(delay).await;
        }
        if let Ok(_guard) = select_update_mutex.try_lock() {
            // 计算 prefer_tcp
            let mut prefer_tcp = false;
            if let Ok(prefer_tcp_var) = std::env::var("PREFER_TCP") {
                if prefer_tcp_var == "1" { prefer_tcp = true; }
            }
            // 重新计算并存储 default_conn_ids
            let new_ids = Self::compute_default_conn_ids_from_conns(peer_node_id, &conns, prefer_tcp).await;
            default_conn_ids.store(Arc::new(new_ids));
            // 通知可能等待 select_conns 的协程
            select_update_notify.notify_waiters();
            return true;
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
        self.refresh_default_conn_ids_with_delay_func(std::time::Duration::from_millis(0)).await;
    }

    // const MAX_CONN_AGE_MS: u64 = 800; // 800ms
    const MAX_CONN_AGE_MS: u64 = 1_200; // 800ms
    // const MAX_CONN_AGE_MS: u64 = 1_500;           // 1.5s
    const TCP_PREFERENCE_DELTA_US: u64 = 50_000;  // 50ms (微秒)

    // 使用 multipath 调度器选择最佳连接
    async fn compute_default_conn_ids_from_conns(
        peer_node_id: PeerId,
        conns: &ConnMap,
        _prefer_tcp: bool,
    ) -> Vec<PeerConnId> {
        if conns.is_empty() {
            return vec![];
        }

        // 构建PathSnapshot列表
        let mut path_snapshots: Vec<(PeerConnId, PathSnapshot)> = Vec::new();
        
        for conn in conns.iter() {
            let conn_ref = conn.value();
            let conn_id = conn.get_conn_id();
            
            // 注意：由于 tunnel 在 PeerConn 中被多层包装为 Any 类型，
            // 直接获取 tunnel metrics 比较困难。
            // 这里先使用 None，让 get_path_snapshot 内部使用 Throughput 估算。
            // 未来可以考虑在 PeerConn 初始化时缓存 tunnel 引用。
            let tunnel_metrics = None;
            
            // 构造PathSnapshot
            let snapshot = conn_ref.get_path_snapshot(conn_id.as_u128() as u64, tunnel_metrics);
            path_snapshots.push((conn_id, snapshot));
        }

        if path_snapshots.is_empty() {
            return vec![];
        }

        // 使用multipath调度器选择
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        
        // 获取流量统计用于flow_bytes_sent
        let flow_bytes_sent = if let Some(first_conn) = conns.iter().next() {
            first_conn.value().get_throughput().get_recent_tx_bytes()
        } else {
            0
        };

        let input = PickInput {
            now_ms,
            packet_len: 1500, // 假设标准MTU
            flow_bytes_sent,
            critical: false,
            paths: path_snapshots.iter().map(|(_, s)| s.clone()).collect(),
        };

        tracing::info!(
            ?peer_node_id,
            "multipath scheduler input: now_ms={}, packet_len={}, flow_bytes_sent={}, paths_count={}",
            input.now_ms,
            input.packet_len,
            input.flow_bytes_sent,
            input.paths.len()
        );

        // 使用默认调度器配置选择
        let scheduler = MultipathScheduler::new(SchedulerConfig::default());
        if let Some(decision) = scheduler.pick_path(&input) {
            let mut selected_ids = vec![];
            
            // 添加primary路径
            if let Some((conn_id, _)) = path_snapshots.iter()
                .find(|(_, s)| s.id == decision.primary) {
                selected_ids.push(*conn_id);
            }
            
            // 添加redundant路径（如果有）
            if let Some(redundant_id) = decision.redundant {
                if let Some((conn_id, _)) = path_snapshots.iter()
                    .find(|(_, s)| s.id == redundant_id) {
                    selected_ids.push(*conn_id);
                }
            }

            tracing::info!(
                ?peer_node_id,
                "multipath selected: primary={}, redundant={:?}, conn_ids={:?}",
                decision.primary,
                decision.redundant,
                selected_ids
            );

            selected_ids
        } else {
            // 没有可用路径，返回第一个作为fallback
            tracing::warn!(?peer_node_id, "multipath scheduler returned no path, using fallback");
            vec![path_snapshots[0].0]
        }
    }

    async fn select_conns(&self) -> Vec<ArcPeerConn> {
        // 快路径：已有默认连接
        let mut default_conn_ids = self.default_conn_ids.load();
        if default_conn_ids.is_empty() {
            if !self.refresh_default_conn_ids_with_delay_func(std::time::Duration::from_millis(0)).await {
                // 等待通知直到有默认连接或连接数为0（无法产生默认连接）
                loop {
                    self.select_update_notify.notified().await;
                    let ids = self.default_conn_ids.load();
                    if !ids.is_empty() || self.conns.is_empty() { break; }
                }
            }
            default_conn_ids = self.default_conn_ids.load();
        }

        // if default_conn_ids.len() > 1 {
        //     let fun = self.refresh_default_conn_ids_with_delay_func(std::time::Duration::from_millis(10));
        //     tokio::spawn(async move {
        //         fun.await;
        //     });
        // }

        // 直接返回缓存的默认连接（不再在此处做健康检查，以减少 get_stats 调用）
        let mut conns = vec![];
        for conn_id in default_conn_ids.iter() {
            if let Some(conn) = self.conns.get(conn_id) {
                conns.push(conn.clone());
            }
        }
        conns
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
        // 并发发送，任意一个成功即返回，不等待其他 future
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
