# tocat-plugins

The plugins compiled into [tocat](https://crates.io/crates/tocat): `tee`,
`rate`, `throttle`, `limit`, `block`, `timeout`, `compress`, `process` and
`wasm`.

You almost certainly want the `tocat` crate instead. This one exists because the
binary depends on it, and is useful directly only when embedding the relay's
plugin set somewhere else.

```rust,ignore
let mut registry = Registry::new();
tocat_plugins::register_native(&mut registry);
```

Every plugin is a module behind a cargo feature, and a feature enables both the
module and whatever optional dependencies it needs: a build without `compress`
never compiles zstd, and one without `wasm` never compiles wasmtime. All are on
by default.

`register_native` is the only thing this crate exports, so no plugin can reach
another and nothing above can reach into any of them.

## Documentation

Each plugin's options and cost model are in the user guide, under `docs/` in the
[repository](https://github.com/lkalir/tocat).

## License

MIT or Apache-2.0, at your option.
