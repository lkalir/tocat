//! Native plugins: implementations compiled into the tocat binary, as opposed
//! to WASM modules loaded at runtime.
//!
//! Both kinds implement [`tocat_api::Plugin`] and are looked up through the
//! same [`Registry`], so the relay cannot tell them apart.
//!
//! Every plugin is a module here, behind a cargo feature, and a feature enables
//! both the module and whatever optional dependencies it needs: a build without
//! `compress` never compiles zstd, and one without `wasm` never compiles
//! wasmtime. A crate boundary would buy nothing that does not already buy.
//!
//! [`register_native`] is the only thing this crate exports. No module can
//! reach another and the binary cannot reach any of them, which is what lets a
//! plugin change shape without anything above noticing.

#[cfg(feature = "base64")]
mod base64;

#[cfg(feature = "block")]
mod block;

#[cfg(feature = "compress")]
mod compress;

#[cfg(feature = "frame")]
mod frame;

#[cfg(feature = "hash")]
mod hash;

#[cfg(feature = "hexify")]
mod hexify;

#[cfg(feature = "limit")]
mod limit;

#[cfg(feature = "process")]
mod process;

#[cfg(feature = "rate")]
mod rate;

#[cfg(feature = "tee")]
mod tee;

#[cfg(feature = "throttle")]
mod throttle;

#[cfg(feature = "timeout")]
mod timeout;

#[cfg(feature = "wasm")]
mod wasm;

use tocat_api::Registry;

/// A registry containing every plugin compiled into this binary.
#[must_use]
pub fn native_registry() -> Registry {
    let mut registry = Registry::new();
    register_native(&mut registry);
    registry
}

/// Add the compiled-in plugins to an existing registry.
///
/// Separate from [`native_registry`] so a host that also loads WASM modules can
/// populate one registry from both sources.
pub fn register_native(registry: &mut Registry) {
    #[cfg(feature = "base64")]
    {
        registry.register(base64::Base64Factory);
        registry.register(base64::Unbase64Factory);
    }

    #[cfg(feature = "block")]
    registry.register(block::BlockFactory);

    #[cfg(feature = "compress")]
    {
        registry.register(compress::CompressFactory);
        registry.register(compress::DecompressFactory);
    }

    #[cfg(feature = "frame")]
    {
        registry.register(frame::FrameFactory);
        registry.register(frame::UnframeFactory);
    }

    #[cfg(feature = "hash")]
    registry.register(hash::HashFactory);

    #[cfg(feature = "hexify")]
    {
        registry.register(hexify::HexifyFactory);
        registry.register(hexify::UnhexifyFactory);
    }

    #[cfg(feature = "limit")]
    registry.register(limit::LimitFactory);

    #[cfg(feature = "process")]
    registry.register(process::ProcessFactory);

    #[cfg(feature = "rate")]
    registry.register(rate::RateFactory);

    #[cfg(feature = "tee")]
    registry.register(tee::TeeFactory);

    #[cfg(feature = "throttle")]
    registry.register(throttle::ThrottleFactory);

    #[cfg(feature = "timeout")]
    registry.register(timeout::TimeoutFactory);

    #[cfg(feature = "wasm")]
    registry.register(wasm::WasmFactory);

    // So clippy doesn't get mad if no features are enabled
    let _ = registry;
}
