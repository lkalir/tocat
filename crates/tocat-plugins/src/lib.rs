//! Native plugins: implementations compiled into the tocat binary, as opposed
//! to WASM modules loaded at runtime.
//!
//! Both kinds implement [`tocat_api::Plugin`] and are looked up through the
//! same [`Registry`], so the relay cannot tell them apart.
//!
//! This crate is a facade. Plugins with no dependencies live here as modules;
//! plugins that bring their own dependency tree get their own crate and are
//! re-registered from here. `tocat-cli` must never name an individual plugin
//! crate. [`register_native`] is the only seam, which is what makes moving a
//! plugin between the two forms a non-event.

#[cfg(feature = "block")]
mod block;

#[cfg(feature = "compress")]
mod compress;

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
    #[cfg(feature = "block")]
    registry.register(block::BlockFactory);

    #[cfg(feature = "compress")]
    {
        registry.register(compress::CompressFactory);
        registry.register(compress::DecompressFactory);
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
