# tocat-wasm-abi

The wire format for [tocat](https://crates.io/crates/tocat) WebAssembly guests:
the outbox a guest writes, the constants it writes into it, and nothing else.

`no_std`, no dependencies, and one definition shared by everything that touches
the ABI. The relay decodes an outbox with this crate,
[`tocat-wasm-sdk`](https://crates.io/crates/tocat-wasm-sdk) writes one with it,
and the C header a C or C++ guest includes is generated from it with cbindgen.

```console
$ cargo add tocat-wasm-abi
```

Most people want `tocat-wasm-sdk`, which wraps this in a trait and a macro.
Reach for this one directly if you are writing a guest in another language, or a
host of your own, and need the layout rather than the ergonomics.

Every wire value has two spellings on purpose. `TOCAT_EMIT_BUFFERED` is the name
C sees and `Emit::Buffered` is the name Rust sees; they cannot disagree, because
the enum's discriminants are the constants. The crate asserts its own layout, so
a change that moves a field fails to compile rather than becoming a stage that
reads nonsense.

## Regenerating the C header

```console
$ ./scripts/regen-abi.sh
$ cargo run -p tocat-wasm-abi --example tocat-abi-header -- --check
```

The generator is an example rather than a binary so that cbindgen stays a
dev-dependency and never appears in what a consumer of this crate resolves. The
generated header is committed, so a C guest builds without a Rust toolchain, and
`--check` is what stops the committed copy drifting.

## Documentation

The ABI is specified in the plugin API section of the book, under `docs/` in the
[repository](https://github.com/lkalir/tocat).

## License

MIT or Apache-2.0, at your option.
