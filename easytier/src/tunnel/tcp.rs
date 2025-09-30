use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::FuturesUnordered;
use tokio::net::{TcpListener, TcpSocket, TcpStream};

use super::TunnelInfo;
use crate::tunnel::common::setup_sokcet2;

use super::{
    check_scheme_and_get_socket_addr,
    common::{wait_for_connect_futures, FramedReader, FramedWriter, TunnelWrapper},
    IpVersion, Tunnel, TunnelError, TunnelListener, TunnelMetrics,
};

const TCP_MTU_BYTES: usize = 2000;

/// TCP tunnel 包装器，实现 get_metrics
struct TcpTunnel {
    inner: TunnelWrapper<
        FramedReader<tokio::net::tcp::OwnedReadHalf>,
        FramedWriter<tokio::net::tcp::OwnedWriteHalf, super::common::TcpZCPacketToBytes>
    >,
    metadata: Arc<TcpTunnelMetadata>,
}

impl Tunnel for TcpTunnel {
    fn split(&self) -> (std::pin::Pin<Box<dyn super::ZCPacketStream>>, std::pin::Pin<Box<dyn super::ZCPacketSink>>) {
        self.inner.split()
    }
    
    fn info(&self) -> Option<TunnelInfo> {
        self.inner.info()
    }
    
    fn get_metrics(&self) -> Option<TunnelMetrics> {
        Some(TunnelMetrics {
            inflight_bytes: self.metadata.get_inflight_bytes(),
        })
    }
}

#[derive(Debug)]
pub struct TcpTunnelListener {
    addr: url::Url,
    listener: Option<TcpListener>,
}

impl TcpTunnelListener {
    pub fn new(addr: url::Url) -> Self {
        TcpTunnelListener {
            addr,
            listener: None,
        }
    }

    async fn do_accept(&mut self) -> Result<Box<dyn Tunnel>, std::io::Error> {
        let listener = self.listener.as_ref().unwrap();
        let (stream, _) = listener.accept().await?;

        if let Err(e) = stream.set_nodelay(true) {
            tracing::warn!(?e, "set_nodelay fail in accept");
        }

        let info = TunnelInfo {
            tunnel_type: "tcp".to_owned(),
            local_addr: Some(self.local_url().into()),
            remote_addr: Some(
                super::build_url_from_socket_addr(&stream.peer_addr()?.to_string(), "tcp").into(),
            ),
        };

        // 在分割前保存socket元数据
        let metadata = Arc::new(TcpTunnelMetadata::new(&stream));
        
        let (r, w) = stream.into_split();
        let inner = TunnelWrapper::new(
            FramedReader::new(r, TCP_MTU_BYTES),
            FramedWriter::new(w),
            Some(info),
        );
        
        Ok(Box::new(TcpTunnel {
            inner,
            metadata,
        }))
    }
}

#[async_trait]
impl TunnelListener for TcpTunnelListener {
    async fn listen(&mut self) -> Result<(), TunnelError> {
        self.listener = None;
        let addr =
            check_scheme_and_get_socket_addr::<SocketAddr>(&self.addr, "tcp", IpVersion::Both)
                .await?;

        let socket2_socket = socket2::Socket::new(
            socket2::Domain::for_address(addr),
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )?;
        setup_sokcet2(&socket2_socket, &addr)?;
        let socket = TcpSocket::from_std_stream(socket2_socket.into());

        if let Err(e) = socket.set_nodelay(true) {
            tracing::warn!(?e, "set_nodelay fail in listen");
        }

        self.addr
            .set_port(Some(socket.local_addr()?.port()))
            .unwrap();

        self.listener = Some(socket.listen(1024)?);
        Ok(())
    }

    async fn accept(&mut self) -> Result<Box<dyn Tunnel>, super::TunnelError> {
        loop {
            match self.do_accept().await {
                Ok(ret) => return Ok(ret),
                Err(e) => {
                    use std::io::ErrorKind::*;
                    if matches!(
                        e.kind(),
                        NotConnected | ConnectionAborted | ConnectionRefused | ConnectionReset
                    ) {
                        tracing::warn!(?e, "accept fail with retryable error: {:?}", e);
                        continue;
                    }
                    tracing::warn!(?e, "accept fail");
                    return Err(e.into());
                }
            }
        }
    }

    fn local_url(&self) -> url::Url {
        self.addr.clone()
    }
}

fn get_tunnel_with_tcp_stream(
    stream: TcpStream,
    remote_url: url::Url,
) -> Result<Box<dyn Tunnel>, super::TunnelError> {
    if let Err(e) = stream.set_nodelay(true) {
        tracing::warn!(?e, "set_nodelay fail in get_tunnel_with_tcp_stream");
    }

    let info = TunnelInfo {
        tunnel_type: "tcp".to_owned(),
        local_addr: Some(
            super::build_url_from_socket_addr(&stream.local_addr()?.to_string(), "tcp").into(),
        ),
        remote_addr: Some(remote_url.into()),
    };

    // 在分割前保存socket元数据
    let metadata = Arc::new(TcpTunnelMetadata::new(&stream));
    
    let (r, w) = stream.into_split();
    let inner = TunnelWrapper::new(
        FramedReader::new(r, TCP_MTU_BYTES),
        FramedWriter::new(w),
        Some(info),
    );
    
    Ok(Box::new(TcpTunnel {
        inner,
        metadata,
    }))
}

#[derive(Debug)]
pub struct TcpTunnelConnector {
    addr: url::Url,

    bind_addrs: Vec<SocketAddr>,
    ip_version: IpVersion,
}

impl TcpTunnelConnector {
    pub fn new(addr: url::Url) -> Self {
        TcpTunnelConnector {
            addr,
            bind_addrs: vec![],
            ip_version: IpVersion::Both,
        }
    }

    async fn connect_with_default_bind(
        &mut self,
        addr: SocketAddr,
    ) -> Result<Box<dyn Tunnel>, super::TunnelError> {
        tracing::info!(url = ?self.addr, ?addr, "connect tcp start, bind addrs: {:?}", self.bind_addrs);
        let stream = TcpStream::connect(addr).await?;
        tracing::info!(url = ?self.addr, ?addr, "connect tcp succ");
        get_tunnel_with_tcp_stream(stream, self.addr.clone())
    }

    async fn connect_with_custom_bind(
        &mut self,
        addr: SocketAddr,
    ) -> Result<Box<dyn Tunnel>, super::TunnelError> {
        let futures = FuturesUnordered::new();

        for bind_addr in self.bind_addrs.iter() {
            tracing::info!(bind_addr = ?bind_addr, ?addr, "bind addr");

            let socket2_socket = socket2::Socket::new(
                socket2::Domain::for_address(addr),
                socket2::Type::STREAM,
                Some(socket2::Protocol::TCP),
            )?;

            if let Err(e) = setup_sokcet2(&socket2_socket, bind_addr) {
                tracing::error!(bind_addr = ?bind_addr, ?addr, "bind addr fail: {:?}", e);
                continue;
            }

            let socket = TcpSocket::from_std_stream(socket2_socket.into());
            futures.push(socket.connect(addr));
        }

        let ret = wait_for_connect_futures(futures).await;
        get_tunnel_with_tcp_stream(ret?, self.addr.clone())
    }
}

#[async_trait]
impl super::TunnelConnector for TcpTunnelConnector {
    async fn connect(&mut self) -> Result<Box<dyn Tunnel>, super::TunnelError> {
        let addr =
            check_scheme_and_get_socket_addr::<SocketAddr>(&self.addr, "tcp", self.ip_version)
                .await?;
        if self.bind_addrs.is_empty() {
            self.connect_with_default_bind(addr).await
        } else {
            self.connect_with_custom_bind(addr).await
        }
    }

    fn remote_url(&self) -> url::Url {
        self.addr.clone()
    }

    fn set_bind_addrs(&mut self, addrs: Vec<SocketAddr>) {
        self.bind_addrs = addrs;
    }

    fn set_ip_version(&mut self, ip_version: IpVersion) {
        self.ip_version = ip_version;
    }
}

#[cfg(test)]
mod tests {
    use crate::tunnel::{
        common::tests::{_tunnel_bench, _tunnel_pingpong},
        TunnelConnector,
    };

    use super::*;

    #[tokio::test]
    async fn tcp_pingpong() {
        let listener = TcpTunnelListener::new("tcp://0.0.0.0:31011".parse().unwrap());
        let connector = TcpTunnelConnector::new("tcp://127.0.0.1:31011".parse().unwrap());
        _tunnel_pingpong(listener, connector).await
    }

    #[tokio::test]
    async fn tcp_bench() {
        let listener = TcpTunnelListener::new("tcp://0.0.0.0:31012".parse().unwrap());
        let connector = TcpTunnelConnector::new("tcp://127.0.0.1:31012".parse().unwrap());
        _tunnel_bench(listener, connector).await
    }

    #[tokio::test]
    async fn tcp_bench_with_bind() {
        let listener = TcpTunnelListener::new("tcp://127.0.0.1:11013".parse().unwrap());
        let mut connector = TcpTunnelConnector::new("tcp://127.0.0.1:11013".parse().unwrap());
        connector.set_bind_addrs(vec!["127.0.0.1:0".parse().unwrap()]);
        _tunnel_pingpong(listener, connector).await
    }

    #[tokio::test]
    #[should_panic]
    async fn tcp_bench_with_bind_fail() {
        let listener = TcpTunnelListener::new("tcp://127.0.0.1:11014".parse().unwrap());
        let mut connector = TcpTunnelConnector::new("tcp://127.0.0.1:11014".parse().unwrap());
        connector.set_bind_addrs(vec!["10.0.0.1:0".parse().unwrap()]);
        _tunnel_pingpong(listener, connector).await
    }

    #[tokio::test]
    async fn bind_same_port() {
        let mut listener = TcpTunnelListener::new("tcp://[::]:31014".parse().unwrap());
        let mut listener2 = TcpTunnelListener::new("tcp://0.0.0.0:31014".parse().unwrap());
        listener.listen().await.unwrap();
        listener2.listen().await.unwrap();
    }

    #[tokio::test]
    async fn ipv6_pingpong() {
        let listener = TcpTunnelListener::new("tcp://[::1]:31015".parse().unwrap());
        let connector = TcpTunnelConnector::new("tcp://[::1]:31015".parse().unwrap());
        _tunnel_pingpong(listener, connector).await
    }

    #[tokio::test]
    async fn ipv6_domain_pingpong() {
        let listener = TcpTunnelListener::new("tcp://[::1]:31015".parse().unwrap());
        let mut connector =
            TcpTunnelConnector::new("tcp://test.easytier.top:31015".parse().unwrap());
        connector.set_ip_version(IpVersion::V6);
        _tunnel_pingpong(listener, connector).await;

        let listener = TcpTunnelListener::new("tcp://127.0.0.1:31015".parse().unwrap());
        let mut connector =
            TcpTunnelConnector::new("tcp://test.easytier.top:31015".parse().unwrap());
        connector.set_ip_version(IpVersion::V4);
        _tunnel_pingpong(listener, connector).await;
    }

    #[tokio::test]
    async fn test_alloc_port() {
        // v4
        let mut listener = TcpTunnelListener::new("tcp://0.0.0.0:0".parse().unwrap());
        listener.listen().await.unwrap();
        let port = listener.local_url().port().unwrap();
        assert!(port > 0);

        // v6
        let mut listener = TcpTunnelListener::new("tcp://[::]:0".parse().unwrap());
        listener.listen().await.unwrap();
        let port = listener.local_url().port().unwrap();
        assert!(port > 0);
    }

    #[tokio::test]
    async fn test_tcp_get_metrics() {
        let mut listener = TcpTunnelListener::new("tcp://127.0.0.1:31016".parse().unwrap());
        listener.listen().await.unwrap();
        
        let mut connector = TcpTunnelConnector::new("tcp://127.0.0.1:31016".parse().unwrap());
        
        let (server_tunnel, client_tunnel) = tokio::join!(
            listener.accept(),
            connector.connect()
        );
        
        let server_tunnel = server_tunnel.unwrap();
        let client_tunnel = client_tunnel.unwrap();
        
        // 测试get_metrics
        let metrics = client_tunnel.get_metrics();
        println!("Client tunnel metrics: {:?}", metrics);
        
        let server_metrics = server_tunnel.get_metrics();
        println!("Server tunnel metrics: {:?}", server_metrics);
        
        // 在Linux上应该能获取到metrics
        #[cfg(target_os = "linux")]
        {
            assert!(metrics.is_some());
            assert!(server_metrics.is_some());
        }
    }
}

/// TCP tunnel 的元数据，用于获取 inflight bytes
#[derive(Debug, Clone)]
struct TcpTunnelMetadata {
    raw_fd: i32,
}

impl TcpTunnelMetadata {
    fn new(stream: &TcpStream) -> Self {
        Self {
            raw_fd: stream.as_raw_fd(),
        }
    }
    
    /// 获取TCP socket的发送缓冲区中未确认的字节数（inflight bytes）
    fn get_inflight_bytes(&self) -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            use std::io::Error;
            use nix::libc::{c_int, ioctl};
            
            // SIOCOUTQ: 获取发送队列中的字节数
            // 定义见 /usr/include/asm-generic/sockios.h
            const SIOCOUTQ: u64 = 0x5411;
            
            let mut bytes: c_int = 0;
            let ret = unsafe {
                ioctl(self.raw_fd, SIOCOUTQ as _, &mut bytes)
            };
            
            if ret == 0 {
                Some(bytes as u64)
            } else {
                tracing::debug!("ioctl SIOCOUTQ failed: {:?}", Error::last_os_error());
                None
            }
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            // 非Linux系统暂不支持
            None
        }
    }
}
