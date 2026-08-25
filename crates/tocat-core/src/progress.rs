use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Instant,
};

use tocat_api::Direction;
use tokio::io::{AsyncRead, ReadBuf};

use crate::endpoint::ReadHalf;

/// The shared byte count behind the display.
pub struct Meter {
    /// Indexed by [`slot`]: one counter per path.
    counts: [AtomicU64; 2],
    connections: AtomicUsize,
    started: Instant,
    /// Total bytes expected on the forward path, when that is knowable. Absent
    /// means no bar, no percentage and no ETA: the `pv`-on-a-pipe display.
    expected: Option<u64>,
}

fn slot(direction: Direction) -> usize {
    match direction {
        Direction::SourceToSink => 0,
        Direction::SinkToSource => 1,
    }
}

impl Meter {
    pub fn new(expected: Option<u64>) -> Self {
        Self {
            counts: [AtomicU64::new(0), AtomicU64::new(0)],
            connections: AtomicUsize::new(0),
            started: Instant::now(),
            expected,
        }
    }

    /// A handle that adds to one path's count.
    #[must_use]
    pub fn counter(self: &Arc<Self>, direction: Direction) -> Counter {
        Counter {
            meter: Arc::clone(self),
            slot: slot(direction),
        }
    }

    /// Register a live connection, until the guard is dropped.
    #[must_use]
    pub fn connected(self: &Arc<Self>) -> ConnectionGuard {
        self.connections.fetch_add(1, Ordering::Relaxed);
        ConnectionGuard(Arc::clone(self))
    }

    /// `(source-to-sink, sink-to-source)`.
    pub fn read(&self) -> (u64, u64) {
        (
            self.counts[0].load(Ordering::Relaxed),
            self.counts[1].load(Ordering::Relaxed),
        )
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }

    pub fn started(&self) -> Instant {
        self.started
    }

    pub fn expected(&self) -> Option<u64> {
        self.expected
    }

    pub fn load_forward_count(&self) -> u64 {
        self.counts[0].load(Ordering::Relaxed)
    }

    pub fn load_reverse_count(&self) -> u64 {
        self.counts[1].load(Ordering::Relaxed)
    }
}

/// Adds to one path's byte count. Cheap to clone and to call.
#[derive(Clone)]
pub struct Counter {
    meter: Arc<Meter>,
    slot: usize,
}

impl Counter {
    pub fn add(&self, bytes: u64) {
        // Relaxed: a monotonic counter read for display orders nothing.
        self.meter.counts[self.slot].fetch_add(bytes, Ordering::Relaxed);
    }
}

pub struct ConnectionGuard(Arc<Meter>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Counts the bytes read through it.
pub struct Counted<R> {
    inner: R,
    counter: Counter,
}

impl<R: AsyncRead + Unpin> AsyncRead for Counted<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);

        // Counted on `Ready(Err(_))` too: a read can fill part of the buffer
        // and then fail, and those bytes did arrive.
        if poll.is_ready() {
            let read = buf.filled().len().saturating_sub(before);

            if read > 0 {
                this.counter.add(read as u64);
            }
        }

        poll
    }
}

/// Attach counting to a read half.
///
/// A stream is wrapped, so the count happens inside `poll_read` wherever the
/// bytes are eventually read. A datagram socket has nothing to wrap (`pump`
/// calls `recv` on it directly, and the write half is a clone of the same
/// socket) so the counter is handed back for the pump to use instead.
/// Exactly one of the two happens, which is what stops the two paths from
/// counting the same bytes twice.
pub fn count(
    meter: Option<&Arc<Meter>>,
    half: ReadHalf,
    direction: Direction,
) -> (ReadHalf, Option<Counter>) {
    let Some(meter) = meter else {
        return (half, None);
    };

    let counter = meter.counter(direction);

    match half {
        ReadHalf::Stream(reader) => (
            ReadHalf::Stream(Box::new(Counted {
                inner: reader,
                counter,
            })),
            None,
        ),
        ReadHalf::Datagram(socket) => (ReadHalf::Datagram(socket), Some(counter)),
    }
}
