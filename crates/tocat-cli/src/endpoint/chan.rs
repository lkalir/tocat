//! chan.rs: `chan:`, a queue an embedder drives.
//!
//! The transport nothing outside this process can reach. A caller that has
//! linked the library registers a named channel, hands the name to a relay in
//! the ordinary way (`chan:input` on the command line, `source = "chan:input"`
//! in a config file), and then reads and writes the other end. It exists so
//! that a frontend other than the CLI does not have to reimplement `Relay`, and
//! so that a test can drive a path without a kernel object.
//!
//! **Messages, not bytes.** A queue entry is one message and stays one, which
//! makes this a datagram endpoint: it is under the same boundary checks as
//! `udp:` and a stage that cannot carry boundaries is reported here the same
//! way. A byte stream needs none of this, since `tokio::io::duplex` already
//! produces a pair of halves that go straight into [`EndpointStream::Duplex`].
//!
//! **Why a name and not a handle.** [`EndpointSpec`] is `Debug`, `Serialize`
//! and `Deserialize`, which is what `--dump-config` and the config file are
//! built on; a variant carrying a `Sender` would take all three away. The
//! registry holds the handles and the spec holds only the name.
//!
//! [`EndpointSpec`]: crate::endpoint::EndpointSpec
//!
//! # Things that are easy to lose in a refactor
//!
//! Registration is consumed by the connect: the relay side of the pair is
//! taken out of the registry, so a name serves one connection and a second
//! attempt is an error rather than a silent second reader of a queue that has
//! already been drained. That is also why `fork` and reconnection are refused
//! at parse time: both mean opening the same endpoint more than once.
//!
//! The queue is bounded. An unbounded one would let a slow sink turn into
//! unbounded memory, which is the one thing a relay exists to avoid: `send`
//! waits, and that backpressure is what reaches the peer at the other end of
//! the path.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use anyhow::bail;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tocat_api::normalize;
use tokio::sync::{Mutex as AsyncMutex, mpsc};

use crate::endpoint::{
    Connection, EndpointStream, MessageSocket,
    parse::{Opt, ParseEndpointError},
};

/// Messages held in each direction before a send waits.
const DEFAULT_CAPACITY: usize = 64;

/// One message. A `Vec` rather than a borrowed slice because it outlives the
/// call that produced it, and a copy per message is what a queue costs.
pub type Message = Vec<u8>;

/// The embedder's end of a registered channel.
///
/// The relay's end is in the registry until it connects. Dropping `tx` is end
/// of stream to the relay, exactly as a peer closing a socket would be; `rx`
/// ends when the relay's path does.
pub struct Channel {
    /// Into the relay: what the source of a relay reads.
    pub tx: mpsc::Sender<Message>,
    /// Out of the relay: what the sink of a relay wrote.
    pub rx: mpsc::Receiver<Message>,
}

/// The relay's end, waiting to be claimed by a connect.
struct Pending {
    rx: mpsc::Receiver<Message>,
    tx: mpsc::Sender<Message>,
}

/// One namespace per process.
///
/// Global rather than threaded through `Relay::new` and every endpoint's
/// connect, which would put a registry parameter in the signature of code that
/// has nothing to do with channels. The cost is that two relays in one process
/// share the namespace and have to agree on names.
fn registry() -> &'static Mutex<HashMap<String, Pending>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a channel under `name` and take the embedder's end of it.
///
/// Registering over a name that is already there replaces it, and whatever was
/// registered before is dropped: a relay that had not yet connected to it will
/// fail to, and one that had already connected keeps the handles it took.
pub fn register(name: impl Into<String>, capacity: usize) -> Channel {
    let capacity = capacity.max(1);

    let (into_relay, relay_reads) = mpsc::channel(capacity);
    let (relay_writes, out_of_relay) = mpsc::channel(capacity);

    registry().lock().expect("channel registry").insert(
        name.into(),
        Pending {
            rx: relay_reads,
            tx: relay_writes,
        },
    );

    Channel {
        tx: into_relay,
        rx: out_of_relay,
    }
}

/// Drop a registration that no relay claimed.
///
/// Returns whether there was one. A claimed channel is not affected: its
/// handles left the registry when the relay connected.
pub fn unregister(name: &str) -> bool {
    registry()
        .lock()
        .expect("channel registry")
        .remove(name)
        .is_some()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Chan {
    /// The name it was registered under.
    pub channel: String,
    #[serde(default)]
    pub name: Option<String>,
}

impl Chan {
    const SCHEME: &'static str = "chan";

    pub(in crate::endpoint) fn parse<'a>(
        body: &str,
        opts: impl Iterator<Item = Opt<'a>>,
    ) -> Result<Self, ParseEndpointError> {
        if body.is_empty() {
            return Err(ParseEndpointError::Empty);
        }

        let mut name = None;

        for opt in opts {
            match normalize(opt.key).as_str() {
                "name" => name = Some(opt.string()?),
                _ => return Err(opt.unsupported(Self::SCHEME)),
            }
        }

        Ok(Self {
            channel: body.to_owned(),
            name,
        })
    }

    pub(in crate::endpoint) fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("chan://{}", self.channel))
    }

    /// Claim the registered end.
    ///
    /// The failure here is an embedder that named a channel it never
    /// registered, or registered and then connected to twice, and both are
    /// worth an error rather than a wait for a peer that cannot arrive.
    pub(in crate::endpoint) async fn connect(&self) -> anyhow::Result<Connection> {
        let pending = registry()
            .lock()
            .expect("channel registry")
            .remove(&self.channel);

        let Some(pending) = pending else {
            bail!(
                "no channel registered as {:?}; register it before the relay connects, and note \
                 that a channel serves one connection",
                self.channel,
            );
        };

        Ok(EndpointStream::message(Queue {
            rx: AsyncMutex::new(pending.rx),
            tx: Mutex::new(Some(pending.tx)),
        })
        .into_connection())
    }
}

/// The relay's side of a registered channel.
struct Queue {
    /// `recv` needs `&mut` and [`MessageSocket`] hands out `&`. Only the pump
    /// reads a path, so this is never contended.
    rx: AsyncMutex<mpsc::Receiver<Message>>,
    /// Taken by `finish`: dropping the sender is how the embedder's `rx` sees
    /// end of stream, and there is no other way to say it.
    tx: Mutex<Option<mpsc::Sender<Message>>>,
}

impl MessageSocket for Queue {
    /// A message longer than `buf` is truncated, as it is on a datagram
    /// socket: the copy buffer is the message size limit on every message
    /// endpoint, and reporting it differently here would make `-b` mean two
    /// things.
    fn recv<'a>(&'a self, buf: &'a mut [u8]) -> BoxFuture<'a, std::io::Result<Option<usize>>> {
        Box::pin(async move {
            let Some(message) = self.rx.lock().await.recv().await else {
                return Ok(None);
            };

            let n = message.len().min(buf.len());
            buf[..n].copy_from_slice(&message[..n]);

            Ok(Some(n))
        })
    }

    fn send<'a>(&'a self, buf: &'a [u8]) -> BoxFuture<'a, std::io::Result<usize>> {
        Box::pin(async move {
            let sender = self.tx.lock().expect("channel sender").clone();

            let Some(sender) = sender else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "the channel is finished",
                ));
            };

            match sender.send(buf.to_vec()).await {
                Ok(()) => Ok(buf.len()),
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "the other end of the channel is gone",
                )),
            }
        })
    }

    fn finish(&self) {
        drop(self.tx.lock().expect("channel sender").take());
    }
}
