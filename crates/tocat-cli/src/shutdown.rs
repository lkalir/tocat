//! shutdown.rs: draining on signal.
//!
//! The first SIGINT/SIGTERM asks the relay to stop accepting and drain what is
//! in flight; a second one exits immediately. Async paths observe this by
//! awaiting [`Shutdown::recv`] in a `select!`. The blocking copy path cannot.
//! A `spawn_blocking` task is not cancellable, so it polls
//! [`Shutdown::is_triggered`] between chunks instead.

use tokio::sync::watch;
use tracing::{info, warn};

#[derive(Clone)]
pub struct Shutdown(watch::Receiver<bool>);

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

pub fn install() -> Shutdown {
    let (tx, rx) = watch::channel(false);

    tokio::spawn(async move {
        wait_for_signal().await;
        info!("shutdown requested, draining connections (signal again to exit now)");
        let _ = tx.send(true);

        wait_for_signal().await;
        warn!("second signal, exiting immediately");
        std::process::exit(130);
    });

    Shutdown(rx)
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
