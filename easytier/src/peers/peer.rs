use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::{DashMap, DashSet};

use tokio::{select, sync::mpsc};

use tracing::Instrument;

use super::{
    peer_conn::{PeerConn, PeerConnId},
    PacketRecvChan,
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

    default_conn_ids: Arc<ArcSwap<Vec<PeerConnId>>>,
    // 新增：控制选择更新的互斥和通知，替换原清理任务
    select_update_mutex: Arc<tokio::sync::Mutex<()>>, // 保证同一时间只有一个更新任务在运行
    select_update_notify: Arc<tokio::sync::Notify>,    // 通知等待者更新已完成
    default_conn_refresh_task: ScopedTask<()>,         // 200ms 周期刷新任务
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

        Peer {
            peer_node_id,
            conns: conns.clone(),
            packet_recv_chan,
            global_ctx,

            close_event_sender,
            close_event_listener,

            shutdown_notifier,
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

    // 基于现有连接计算默认连接ID集合：
    // - 有健康连接则返回单个（按延迟/偏好排序后的 primary）
    // - 无健康连接则返回最近 3 个（可能 <3）
    // - 若返回数量>1，且其中存在健康连接，则收敛为1个
    async fn compute_default_conn_ids_from_conns(
        peer_node_id: PeerId,
        conns: &ConnMap,
        prefer_tcp: bool,
    ) -> Vec<PeerConnId> {
        // 收集调试信息 + 健康候选（age <= MAX_CONN_AGE_MS）
        struct Cand {
            id: PeerConnId,
            latency_us: u64,
            is_tcp: bool,
        }

        let mut dbg_info: String = String::new();
        let mut cands: Vec<Cand> = Vec::new();
        let mut all_conn_infos: Vec<(PeerConnId, PeerConnInfo)> = Vec::new();

        for conn in conns.iter() {
            let info = conn.value().get_conn_info();
            let stats = match info.stats.as_ref() { Some(s) => s, None => continue };
            let tunnel_type = match info.tunnel.as_ref() { Some(t) => t.tunnel_type.as_str(), None => "" };
            dbg_info.push_str(&format!(
                "[id={} proto={} latency={}us age={}ms] ",
                conn.get_conn_id(),
                tunnel_type,
                stats.latency_us,
                stats.last_response_age_ms
            ));

            let is_tcp = tunnel_type == "tcp";
            if stats.last_response_age_ms <= Peer::MAX_CONN_AGE_MS {
                cands.push(Cand { id: conn.get_conn_id(), latency_us: stats.latency_us, is_tcp });
            }
            all_conn_infos.push((conn.get_conn_id(), info));
        }

        let mut out: Vec<PeerConnId> = Vec::new();

        if !cands.is_empty() {
            // 有健康候选：按延迟 + TCP 偏好选择 primary，并仅返回1个
            cands.sort_by_key(|c| c.latency_us);
            let best_overall_latency = cands[0].latency_us;
            let best_tcp_idx_and_latency = cands
                .iter()
                .enumerate()
                .filter(|(_, c)| c.is_tcp)
                .min_by_key(|(_, c)| c.latency_us)
                .map(|(i, c)| (i, c.latency_us));
            let mut primary_idx = 0usize;
            if prefer_tcp {
                if let Some((tcp_idx, best_tcp_latency)) = best_tcp_idx_and_latency {
                    let diff = if best_overall_latency > best_tcp_latency {
                        best_overall_latency - best_tcp_latency
                    } else {
                        best_tcp_latency - best_overall_latency
                    };
                    if diff < Peer::TCP_PREFERENCE_DELTA_US {
                        primary_idx = tcp_idx;
                    }
                }
            }
            out = cands.into_iter().map(|c| c.id).collect();
            if primary_idx != 0 {
                let chosen = out.remove(primary_idx);
                out.insert(0, chosen);
            }
            out.truncate(1);
        } else {
            // 无健康候选：返回按最近响应排序的最多3个
            all_conn_infos.sort_by_key(|(_, info)| info.stats.as_ref().map_or(u64::MAX, |s| s.latency_us));
            out = all_conn_infos.iter().map(|(id, _)| *id).take(3).collect();

            // // 额外检查：仅检查 out 中的 id，利用已收集的 PeerConnInfo，避免额外 get_stats
            // if out.len() > 1 {
            //     let mut age_map: std::collections::HashMap<PeerConnId, u64> = std::collections::HashMap::new();
            //     for (id, info) in &all_conn_infos {
            //         if let Some(s) = info.stats.as_ref() {
            //             age_map.insert(*id, s.last_response_age_ms);
            //         }
            //     }
            //     let mut has_healthy = false;
            //     for id in out.iter() {
            //         if let Some(age) = age_map.get(id) {
            //             if *age <= Peer::MAX_CONN_AGE_MS { has_healthy = true; break; }
            //         }
            //     }
            //     if has_healthy {
            //         out.truncate(1);
            //     }
            // }
        }

        tracing::info!(
            ?peer_node_id,
            prefer_tcp,
            "selecting default peer conns from: {} => {:?}",
            dbg_info,
            out,
        );

        out
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
