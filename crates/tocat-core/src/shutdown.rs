//! shutdown.rs: draining on signal.
//!
//! The first SIGINT/SIGTERM asks the relay to stop accepting and drain what is
//! in flight; a second one exits immediately.
//!
//! Draining means the signal is delivered *as end of stream*, not as
//! cancellation: a handle goes down to the reads that touch an endpoint, which
//! stop returning chunks, and the ordinary end-of-stream path takes it from
//! there, so stages still get `on_eof` and still get their last bytes written.
//! Racing a copy against [`Shutdown::recv`] in a `select!` is only correct
//! where nothing is declared and nothing is owed a flush.
//!
//! The blocking copy path can do neither. A `spawn_blocking` task is not
//! cancellable, so it polls [`Shutdown::is_triggered`] between chunks instead.

use tokio::sync::watch;
use tracing::{info, warn};

#[derive(Clone)]
pub struct Shutdown(watch::Receiver<bool>);

/// The sending half of a [`Shutdown`], for a caller that decides on its own
/// when to drain.
///
/// [`install`] holds one inside its signal task. Tests hold one directly,
/// which is the only way to reach the drain path without sending the process a
/// signal.
pub struct Trigger(watch::Sender<bool>);

impl Trigger {
    /// Ask every holder of the paired [`Shutdown`] to drain, exactly as the
    /// first signal does.
    pub fn drain(&self) {
        let _ = self.0.send(true);
    }
}

impl Shutdown {
    pub async fn recv(&mut self) {
        if *self.0.borrow() {
            return;
        }

        let _ = self.0.changed().await;
    }

    /// Non-blocking check, for code that cannot await.
    #[must_use]
    pub fn is_triggered(&self) -> bool {
        *self.0.borrow()
    }
}

/// A [`Shutdown`] and its [`Trigger`], with no signal handler attached.
///
/// The trigger has to outlive every [`Shutdown`] clone. Dropping the last
/// sender closes the channel, [`Shutdown::recv`] returns when that happens, and
/// a closed channel is therefore indistinguishable from a signal: a caller that
/// drops the trigger early drains the relay it just started.
pub fn channel() -> (Trigger, Shutdown) {
    let (tx, rx) = watch::channel(false);

    (Trigger(tx), Shutdown(rx))
}

pub fn install() -> Shutdown {
    let (trigger, rx) = channel();

    tokio::spawn(async move {
        wait_for_signal().await;
        info!("shutdown requested, draining connections (signal again to exit now)");
        trigger.drain();

        wait_for_signal().await;
        warn!("second signal, exiting immediately");
        std::process::exit(130);
    });

    rx
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");

    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
