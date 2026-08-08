# tocat-wasm-shell

A REPL for poking at a [tocat](https://crates.io/crates/tocat) WebAssembly guest
without a relay: load a module, call its exports by hand, and decode the outbox
after each call.

```console
$ cargo run -p tocat-wasm-shell -- --path upper.wasm
wasm> send hello
```

A guest is easiest to debug this way, since there are no imports to satisfy and
no relay to stand up. The usual first thing it catches is a guest handing the
host offsets into its own arena rather than addresses in linear memory, which
shows up as an outbox that decodes as all zeroes.

## License

MIT or Apache-2.0, at your option.
