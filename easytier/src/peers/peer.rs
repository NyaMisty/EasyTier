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
    default_conn_id_clear_task: ScopedTask<()>,
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

        let conns_copy = conns.clone();
        let shutdown_notifier_copy = shutdown_notifier.clone();
        let global_ctx_copy = global_ctx.clone();
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

        let default_conn_ids = Arc::new(ArcSwap::from_pointee(Vec::new()));

        let conns_copy = conns.clone();
        let default_conn_ids_copy = default_conn_ids.clone();
        let default_conn_id_clear_task = ScopedTask::from(tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                if conns_copy.len() > 1 {
                    default_conn_ids_copy.store(Arc::new(vec![PeerConnId::default()]));
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
            default_conn_id_clear_task,
        }
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
    }

    const MAX_CONN_AGE_MS: u64 = 800; // 800ms
    // const MAX_CONN_AGE_MS: u64 = 1_500;           // 1.5s
    const TCP_PREFERENCE_DELTA_US: u64 = 50_000;  // 50ms (微秒)

    async fn _do_select_conn(&self, filter_old: bool, prefer_tcp: bool) -> Vec<PeerConnId> {

        #[derive(Clone, Debug)]
        struct Cand {
            id: PeerConnId,
            latency_us: u64,
            is_tcp: bool,
            conn_info: PeerConnInfo,
        }

        let mut cands: Vec<Cand> = Vec::new();

        // 单次遍历收集候选，避免不必要的 clone
        for conn in self.conns.iter() {
            let info = conn.value().get_conn_info();
            let stats = match info.stats.as_ref() {
                Some(s) => s,
                None => continue,
            };
            if filter_old && stats.last_response_age_ms > Peer::MAX_CONN_AGE_MS {
                continue;
            }

            let is_tcp = match info.tunnel.as_ref() {
                Some(t) => t.tunnel_type.as_str() == "tcp",
                None => false,
            };

            cands.push(Cand {
                id: conn.get_conn_id(),
                latency_us: stats.latency_us,
                is_tcp,
                conn_info: info,
            });
        }

        tracing::debug!(
            ?self.peer_node_id,
            filter_old,
            prefer_tcp,
            conn_count = self.conns.len(),
            "selecting peer conns from: {:?}",
            cands,
        );


        if cands.is_empty() {
            return Vec::new();
        }

        // 先按延迟升序排序，得到全局最优在 index 0
        cands.sort_by_key(|c| c.latency_us);

        let best_overall_latency = cands[0].latency_us;

        // 找到最优 TCP（如果有）
        let best_tcp_idx_and_latency = cands
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_tcp)
            .min_by_key(|(_, c)| c.latency_us)
            .map(|(i, c)| (i, c.latency_us));

        // 确定置顶元素（primary）
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

        // 生成返回列表：把 primary 移到最前，其余保持延迟升序
        let total_candidates = cands.len();
        let primary_info = cands[primary_idx].conn_info.clone();
        let mut out: Vec<PeerConnId> = cands.into_iter().map(|c| c.id).collect();
        if primary_idx != 0 {
            let chosen = out.remove(primary_idx);
            out.insert(0, chosen);
        }

        // 精简日志，避免使用已移动的数据
        tracing::debug!(
            "peer {} selected primary conn (tcp_pref={}, total_candidates={}): {:?}",
            self.peer_node_id,
            prefer_tcp,
            total_candidates,
            primary_info,
        );

        out
    }

    async fn _do_select_conn_recent(&self) -> Vec<PeerConnId> {
        let mut conn_infos: Vec<(PeerConnId, PeerConnInfo)> = vec![];
        for conn in self.conns.iter() {
            let conn_info = conn.value().get_conn_info();
            // let stats = match conn_info.stats.as_ref() { Some(s) => s, None => continue };
            // if stats.last_response_age_ms > Peer::MAX_CONN_AGE_MS { continue; }
            conn_infos.push((conn.get_conn_id(), conn_info));
        }
        // sort by last_response_age_ms
        conn_infos.sort_by_key(|(_, info)| info.stats.as_ref().map_or(u64::MAX, |s| s.last_response_age_ms));
        conn_infos.into_iter().map(|(id, _)| id).collect()
    }

    async fn select_conns(&self) -> Vec<ArcPeerConn> {
        let default_conn_ids = self.default_conn_ids.load();
        if !default_conn_ids.is_empty() {
            let mut conns = vec![];
            for conn_id in default_conn_ids.iter() {
                if let Some(conn) = self.conns.get(conn_id) {
                    conns.push(conn.clone());
                }
            }
            if !conns.is_empty() {
                return conns;
            }
        }

        let conn_ids = self._do_select_conn(true, true).await;
        if !conn_ids.is_empty() {
            self.default_conn_ids.store(Arc::new(conn_ids.iter().take(1).cloned().collect()));
        } else {
            let conn_ids = self._do_select_conn_recent().await;
            tracing::warn!("peer {} has no healthy conn, selecting 3 most recent conns!", self.peer_node_id);
            self.default_conn_ids.store(Arc::new(conn_ids.iter().take(3).cloned().collect()));
        }

        let mut conns = vec![];
        let default_conn_ids = self.default_conn_ids.load();
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
