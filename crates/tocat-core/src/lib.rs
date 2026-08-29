//! The relay: endpoints, the layers stacked over them, and the plugin pipeline
//! that runs between two of them.
//!
//! Everything here is what a frontend drives rather than what a frontend is.
//! The command line, the logging setup and the progress display live in the
//! `tocat` binary, and this crate depends on none of them, which is the whole
//! point of the split: an embedder pays for the relay and not for the terminal.
//!
//! The entry point is [`relay::Relay`]. Build one from two
//! [`endpoint::EndpointSpec`]s and a plugin chain, then `run` it with a
//! [`shutdown::Shutdown`].

pub mod buffer;
pub mod child;
pub mod endpoint;
pub mod host;
pub mod progress;
pub mod pump;
pub mod relay;
pub mod shutdown;
pub mod spec;

pub mod plugins {
    pub use tocat_plugins::{native_registry, register_native};
}
