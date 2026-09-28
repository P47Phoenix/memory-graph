//! Per-connection identity for the snapshot handle table (ADR 0004 D1):
//! every accepted TCP stream is wrapped in [`ConnIo`], whose `Connected`
//! impl attaches a [`ConnInfo`] (a server-assigned connection id plus the
//! peer address) to every request on that connection as a tonic extension.
//! When the connection closes, hyper drops the `ConnIo`, and its `Drop`
//! drops every snapshot handle that connection opened.
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Weak;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tonic::transport::server::Connected;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnInfo {
    pub id: u64,
    pub peer: Option<SocketAddr>,
}

pub struct ConnIo {
    inner: TcpStream,
    info: ConnInfo,
    /// The slot whose snapshot table holds this connection's handles.
    slot: Option<Weak<crate::StoreSlot>>,
}

static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

impl ConnIo {
    pub fn new(inner: TcpStream) -> Self {
        let _ = inner.set_nodelay(true);
        let info = ConnInfo {
            id: NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed),
            peer: inner.peer_addr().ok(),
        };
        Self {
            inner,
            info,
            slot: None,
        }
    }

    /// A connection whose snapshot handles in `slot` die with it.
    pub fn for_slot(inner: TcpStream, slot: Weak<crate::StoreSlot>) -> Self {
        let mut c = Self::new(inner);
        c.slot = Some(slot);
        c
    }
}

impl Drop for ConnIo {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.as_ref().and_then(Weak::upgrade) {
            let n = slot.snapshots().drop_conn(self.info.id);
            if n > 0 {
                tracing::debug!(
                    conn = self.info.id,
                    dropped = n,
                    "connection closed; its snapshot handles dropped"
                );
            }
        }
    }
}

impl Connected for ConnIo {
    type ConnectInfo = ConnInfo;
    fn connect_info(&self) -> ConnInfo {
        self.info.clone()
    }
}

impl AsyncRead for ConnIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for ConnIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// The connection id of a request (0 when the transport attached none,
/// which only an in-process test transport would do).
pub fn conn_id<T>(req: &tonic::Request<T>) -> u64 {
    req.extensions().get::<ConnInfo>().map_or(0, |c| c.id)
}
