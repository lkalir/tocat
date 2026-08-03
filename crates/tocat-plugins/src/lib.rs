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
    #[cfg(feature = "tee")]
    registry.register(tocat_plugin_tee::TeeFactory);

    #[cfg(feature = "process")]
    registry.register(tocat_plugin_process::ProcessFactory);

    #[cfg(feature = "rate")]
    registry.register(tocat_plugin_rate::RateFactory);

    #[cfg(feature = "limit")]
    registry.register(tocat_plugin_limit::LimitFactory);

    #[cfg(feature = "throttle")]
    registry.register(tocat_plugin_throttle::ThrottleFactory);

    #[cfg(feature = "compress")]
    {
        registry.register(tocat_plugin_compress::CompressFactory);
        registry.register(tocat_plugin_compress::DecompressFactory);
    }

    // So clippy doesn't get mad if no features are enabled
    let _ = registry;
}
