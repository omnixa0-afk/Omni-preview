//! Bounds on inbound bulk streams (security review 2026-11-09, F5). Each one is handed to the app on
//! an OS thread of its own, and the app reads it synchronously — its head, a request, a whole file.
//! A peer that opened streams and sent nothing used to hold one thread each for as long as it kept
//! the connection up. Now there are at most [`MAX_BULK_WORKERS`] such threads, [`MAX_BULK_WORKERS_PER_PEER`]
//! for one peer, and a read that waits [`BULK_READ_IDLE_LIMIT`] without a byte fails, ending its thread.
//!
//! Both ends of every bulk stream, ours and the peer's, are bounded so (blind audit 2026-11-12, pass
//! 15): a stream we opened waited for a holder's answer for good, and a write to a peer that stopped
//! reading held the serving thread for good — [`bound_bulk_stream`].

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use omni_network::BulkStream;

use crate::MeshNodeAddress;

/// Inbound bulk streams being handled at once, in all.
pub(crate) const MAX_BULK_WORKERS: usize = 128;
/// … and for one peer.
pub(crate) const MAX_BULK_WORKERS_PER_PEER: usize = 32;
/// A read of a bulk stream that gets nothing for this long fails; so does a write the peer takes
/// nothing of for this long.
pub(crate) const BULK_READ_IDLE_LIMIT: Duration = Duration::from_secs(60);

/// `stream` with both halves bounded by [`BULK_READ_IDLE_LIMIT`], timers on `timers`.
pub(crate) fn bound_bulk_stream(stream: BulkStream, timers: &tokio::runtime::Handle) -> BulkStream {
    BulkStream {
        send: Box::new(IdleLimited::new(stream.send, BULK_READ_IDLE_LIMIT, timers.clone())),
        recv: Box::new(IdleLimited::new(stream.recv, BULK_READ_IDLE_LIMIT, timers.clone())),
    }
}

/// The streams being handled now, in all and per peer.
#[derive(Default)]
pub(crate) struct BulkWorkers {
    per_peer: Mutex<HashMap<MeshNodeAddress, usize>>,
}

/// A worker's place, given back when it is dropped (its thread ended).
pub(crate) struct BulkWorkerSlot {
    workers: Arc<BulkWorkers>,
    peer: MeshNodeAddress,
}

impl BulkWorkers {
    /// A place for one more stream of `peer`, or `None` when the caps are reached.
    pub(crate) fn take(self: &Arc<Self>, peer: MeshNodeAddress) -> Option<BulkWorkerSlot> {
        let mut per_peer = self.per_peer.lock();
        let total: usize = per_peer.values().sum();
        let mine = per_peer.get(&peer).copied().unwrap_or(0);
        if total >= MAX_BULK_WORKERS || mine >= MAX_BULK_WORKERS_PER_PEER {
            return None;
        }
        per_peer.insert(peer, mine + 1);
        Some(BulkWorkerSlot { workers: Arc::clone(self), peer })
    }
}

impl Drop for BulkWorkerSlot {
    fn drop(&mut self) {
        let mut per_peer = self.workers.per_peer.lock();
        if let Some(count) = per_peer.get_mut(&self.peer) {
            *count -= 1;
            if *count == 0 {
                per_peer.remove(&self.peer);
            }
        }
    }
}

/// A reader (or writer) that fails with `TimedOut` when a read waits `limit` without a byte — or a
/// write, flush or shutdown waits `limit` without the peer taking any. The wait counts from
/// when a read starts waiting, not from the last byte: a handler that writes for an hour and then
/// reads is not cut off. The timer lives on `timers` (the mesh's runtime, up as long as the stream
/// can be read): the handlers read from short-lived runtimes of their own, and a wait abandoned in
/// one of them must not leave a timer of a runtime that is gone.
pub(crate) struct IdleLimited<R> {
    inner: R,
    limit: Duration,
    timers: tokio::runtime::Handle,
    waiting: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<R> IdleLimited<R> {
    pub(crate) fn new(inner: R, limit: Duration, timers: tokio::runtime::Handle) -> Self {
        Self { inner, limit, timers, waiting: None }
    }
}

impl<R> IdleLimited<R> {
    /// `polled` as it is when ready; when pending, pending until `limit` runs out, then `TimedOut`.
    fn bounded<T>(&mut self, cx: &mut Context<'_>, polled: Poll<std::io::Result<T>>, what: &'static str) -> Poll<std::io::Result<T>> {
        match polled {
            Poll::Ready(result) => {
                self.waiting = None;
                Poll::Ready(result)
            }
            Poll::Pending => {
                let (limit, timers) = (self.limit, &self.timers);
                let sleep = self.waiting.get_or_insert_with(|| {
                    let _on = timers.enter();
                    Box::pin(tokio::time::sleep(limit))
                });
                match sleep.as_mut().poll(cx) {
                    Poll::Ready(()) => {
                        self.waiting = None;
                        Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::TimedOut, what)))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for IdleLimited<R> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_read(cx, buf);
        this.bounded(cx, polled, "the peer sent nothing on the bulk stream")
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for IdleLimited<W> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_write(cx, buf);
        this.bounded(cx, polled, "the peer took nothing of the bulk stream")
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_flush(cx);
        this.bounded(cx, polled, "the peer took nothing of the bulk stream")
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_shutdown(cx);
        this.bounded(cx, polled, "the peer took nothing of the bulk stream")
    }
}

use std::future::Future as _;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt as _;

    /// A stream whose peer sends nothing fails its read after the limit; one that sends goes on.
    #[tokio::test(start_paused = true)]
    async fn a_silent_bulk_stream_fails_its_read_after_the_limit() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let mut limited = IdleLimited::new(reader, Duration::from_secs(60), tokio::runtime::Handle::current());
        let mut byte = [0u8; 1];
        tokio::io::AsyncWriteExt::write_all(&mut writer, b"x").await.unwrap();
        assert_eq!(limited.read(&mut byte).await.unwrap(), 1);
        let error = limited.read(&mut byte).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    /// A write the peer takes nothing of fails after the limit, freeing the thread that serves it
    /// (blind audit 2026-11-12, pass 15); one it takes goes through.
    #[tokio::test(start_paused = true)]
    async fn a_write_the_peer_does_not_take_fails_after_the_limit() {
        use tokio::io::AsyncWriteExt as _;
        let (writer, mut reader) = tokio::io::duplex(8);
        let mut limited = IdleLimited::new(writer, Duration::from_secs(60), tokio::runtime::Handle::current());
        limited.write_all(b"12345678").await.unwrap();
        let mut taken = [0u8; 8];
        reader.read_exact(&mut taken).await.unwrap();
        limited.write_all(b"12345678").await.unwrap();
        let error = limited.write_all(b"more than fits").await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    /// The caps: no more than the per-peer share for one peer, and a slot comes back when dropped.
    #[test]
    fn bulk_workers_are_capped_per_peer_and_given_back() {
        let workers = Arc::new(BulkWorkers::default());
        let peer = MeshNodeAddress::from_bytes([1; 20]);
        let slots: Vec<BulkWorkerSlot> = (0..MAX_BULK_WORKERS_PER_PEER).map(|_| workers.take(peer).expect("under the cap")).collect();
        assert!(workers.take(peer).is_none(), "the peer's share is full");
        assert!(workers.take(MeshNodeAddress::from_bytes([2; 20])).is_some(), "another peer still gets one");
        drop(slots);
        assert!(workers.take(peer).is_some(), "given back");
    }
}
