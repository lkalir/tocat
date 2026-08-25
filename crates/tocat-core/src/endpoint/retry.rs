//! retry.rs: how hard a scheme tries to open its endpoint.
//!
//! One grammar, parsed by each reopenable scheme and applied once in
//! [`EndpointSpec::connect`], so a scheme gets this by holding the struct
//! rather than by writing a loop of its own.
//!
//! This is the *attempts* half of resilience: how many times to try, how long
//! to wait between tries, and how long one try may take. What happens to a
//! pipeline when a connection that was working goes away is a separate
//! question with a separate option, because the two compose: a policy of
//! trying forever is as useful for the first connect, against a server that
//! has not started yet, as it is for the twentieth.
//!
//! [`EndpointSpec::connect`]: crate::endpoint::EndpointSpec::connect
//!
//! # What counts as a failure
//!
//! Only an error opening the endpoint. A peer that closes cleanly has said it
//! is finished, and retrying past that would make `tocat - tcp:host:9000`
//! impossible to end. That rule lives in the relay, where end of stream is
//! seen; everything here is about an open that did not succeed.
//!
//! # Things that are easy to lose in a refactor
//!
//! A listening endpoint has no business here. Its equivalent of retrying is
//! `fork`, which keeps accepting, and a bind that fails is a configuration
//! error rather than something to wait out. The schemes refuse these options
//! at parse time rather than accepting them and doing nothing.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tocat_api::{Interval, normalize};

use crate::endpoint::parse::{Opt, ParseEndpointError};

/// Between one failed attempt and the next, when `interval=` says nothing.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);

/// Between one restart and the next, when `reconnect-delay=` says nothing.
///
/// Not zero. A peer that accepts and immediately resets would otherwise spin
/// the restart loop as fast as a connect completes, and the attempt interval
/// does not cover it: that governs connects that failed, and this one
/// succeeded.
const DEFAULT_RECONNECT_DELAY: Duration = Duration::from_millis(500);

/// How many times to try, counting the first.
///
/// Untagged so that `retry = 3` and `retry = "forever"` both deserialize, as
/// on the command line.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Attempts {
    Limited(u32),
    Forever(Forever),
}

/// The one string `retry` takes instead of a number.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Forever {
    Forever,
}

/// What happens to the pipeline when a connection that was working goes away.
///
/// Separate from the attempt policy above, and orthogonal to it: how hard to
/// try is one question, and what to do with the stages that were mid-stream is
/// another. Only an error triggers this. A peer that closed cleanly has said it
/// is finished.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Continuity {
    /// End the run, which is what a relay has always done.
    #[default]
    None,

    /// Open everything again and start over with fresh stages, as though the
    /// command had been rerun. The old path gets its end of stream first, so a
    /// stage holding bytes hands them over before it is replaced.
    ///
    /// Both ends are reopened, not just the one that failed: a relay is a pair,
    /// and half of a rerun is not a rerun. On a listening source that means the
    /// current client is dropped and the next one accepted.
    Restart,

    /// Reopen underneath the pipeline, so the stages carry on with the state
    /// they had. Only the endpoint that failed is reopened, and only that
    /// endpoint: the other side of the relay never learns anything happened.
    ///
    /// The stream has a hole in it. Bytes the kernel accepted and had not
    /// delivered are gone, and a write interrupted halfway leaves an unknowable
    /// prefix on the wire. Stage state surviving is not the stream surviving.
    Keep,
}

impl Continuity {
    pub fn restarts(self) -> bool {
        self == Continuity::Restart
    }

    pub fn keeps(self) -> bool {
        self == Continuity::Keep
    }
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Retry {
    /// Absent is one attempt: the historical behaviour, where a refused
    /// connection is the end of the run.
    pub retry: Option<Attempts>,

    /// Between attempts. Fixed rather than backing off, because the common use
    /// is waiting for a service to come up and a predictable wait is easier to
    /// reason about than a growing one.
    pub interval: Option<Interval>,

    /// How long one attempt may take before it counts as failed.
    ///
    /// Without this a connect to an address that blackholes packets waits for
    /// the kernel, which is minutes, and `retry` cannot help because the first
    /// attempt never finishes.
    pub connect_timeout: Option<Interval>,

    /// What to do when an established connection fails. See [`Continuity`].
    pub reconnect: Continuity,

    /// A floor on how often a reconnect may happen, separate from `interval`
    /// because the two answer different questions: `interval` waits between
    /// connects that failed, and this waits between connections that worked and
    /// then did not.
    pub reconnect_delay: Option<Interval>,
}

impl Retry {
    /// Take `opt` if it names one of these, and say whether it did.
    ///
    /// The same shape as the socket options: `Ok(false)` leaves the caller to
    /// report an unsupported option against its own scheme name.
    pub(in crate::endpoint) fn option(
        &mut self,
        opt: &Opt<'_>,
    ) -> Result<bool, ParseEndpointError> {
        match normalize(opt.key).as_str() {
            "retry" => self.retry = Some(attempts(opt)?),
            "forever" => {
                self.retry = opt
                    .flag()?
                    .then_some(Attempts::Forever(Forever::Forever))
                    .or(Some(Attempts::Limited(1)));
            }
            "interval" => self.interval = Some(interval(opt)?),
            "connecttimeout" => self.connect_timeout = Some(interval(opt)?),
            "reconnect" => self.reconnect = continuity(opt)?,
            "reconnectdelay" => self.reconnect_delay = Some(interval(opt)?),
            _ => return Ok(false),
        }

        Ok(true)
    }

    /// Whether a failure at `attempt`, counting from one, is worth another go.
    pub(in crate::endpoint) fn again(&self, attempt: u32) -> bool {
        match self.retry {
            None => false,
            Some(Attempts::Forever(_)) => true,
            Some(Attempts::Limited(limit)) => attempt < limit,
        }
    }

    /// How long to wait before the next attempt.
    pub(in crate::endpoint) fn wait(&self) -> Duration {
        self.interval.map_or(DEFAULT_INTERVAL, |i| i.duration())
    }

    /// How long one attempt may take, if a limit was asked for.
    pub(in crate::endpoint) fn deadline(&self) -> Option<Duration> {
        self.connect_timeout.map(|i| i.duration())
    }

    /// How long to wait before reopening after a connection failed.
    pub fn reconnect_delay(&self) -> Duration {
        self.reconnect_delay
            .map_or(DEFAULT_RECONNECT_DELAY, |i| i.duration())
    }
}

/// A count, or the word that means no count.
fn attempts(opt: &Opt<'_>) -> Result<Attempts, ParseEndpointError> {
    let text = opt.text()?;

    if normalize(text) == "forever" {
        return Ok(Attempts::Forever(Forever::Forever));
    }

    Ok(Attempts::Limited(opt.count()?.get() as u32))
}

fn continuity(opt: &Opt<'_>) -> Result<Continuity, ParseEndpointError> {
    match normalize(opt.text()?).as_str() {
        "none" => Ok(Continuity::None),
        "restart" => Ok(Continuity::Restart),
        "keep" => Ok(Continuity::Keep),
        other => Err(ParseEndpointError::InvalidFlag(format!(
            "reconnect={other}, which is none, restart or keep",
        ))),
    }
}

fn interval(opt: &Opt<'_>) -> Result<Interval, ParseEndpointError> {
    opt.text()?
        .parse()
        .map_err(|e| ParseEndpointError::InvalidInterval(format!("{e}")))
}
