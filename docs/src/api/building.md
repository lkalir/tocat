# Options and building

A plugin is registered as a factory, and the factory is what the host talks to
when it resolves an entry.

```rust,ignore
pub trait PluginFactory: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> &str { "" }
    fn execution(&self) -> Execution { Execution::Inline }
    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage>;
}
```

`name` is the spelling users write and `--list-plugins` prints; the registry
stores it normalized, so case, dashes and underscores do not matter at the call
site. `description` is the one line beside it in that listing.

`execution` is the default placement: `Inline` (the default) runs the stage on
the reading task, `Detached` gives it its own task behind a bounded channel,
which buys concurrency with the reader for one copy and one wakeup per unit. An
entry's `detach = true|false` overrides it. Only reach for `Detached` when the
stage does real work per byte, as `compress` does.

`build` runs once per direction per connection. It is where everything expensive
or fallible belongs, so that the per-chunk path is a synchronous call that
either forwards a slice or writes into a buffer.

## BuildCtx

| Call                       | Gives                                                                                                     |
| -------------------------- | --------------------------------------------------------------------------------------------------------- |
| `ctx.config::<T>()?`       | The entry's options, deserialized into the plugin's own config type                                       |
| `ctx.raw_config()`         | Those options as a `serde_json` map, for anything that needs them untyped                                 |
| `ctx.stage()`              | `StageInfo`: this instance's `index`, `total`, display `name`, and its `upstream`/`downstream` neighbours |
| `ctx.meta()`               | `PipelineMeta`: the `direction`, the `source` and `sink` labels, and the `peer` under `fork`              |
| `ctx.direction()`          | Shorthand for `ctx.meta().direction`                                                                      |
| `ctx.name()`               | The plugin name this entry asked for                                                                      |
| `ctx.open_channel(target)` | A [side channel](effects.md) handle                                                                       |

`StageInfo::upstream` and `downstream` are the stage's actual neighbours on this
path: the adjacent stages' display names, or an endpoint label at either end. A
`tee` wedged between two other stages therefore describes the hop it is really
watching rather than the endpoints it is nowhere near. `label()` on either type
formats the pair as `upstream -> downstream`, oriented for this path.

Position cannot change after construction, so anything derived from it should be
computed here and cached. So should the answer to `tick_interval`, which the
host reads exactly once, at the end of construction.

## Config types

Declare a config struct and deserialize it. Nothing about tocat's lenient
matching appears in the plugin:

```rust,ignore
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct BlockConfig {
    #[serde(default = "default_size")]
    pub size: ByteSize,
    #[serde(default)]
    pub flush: Option<Interval>,
    #[serde(default)]
    pub pad: bool,
}
```

`ctx.config()` deserializes through `Forgiving`, which matches option keys and
enum values the way every other identifier in tocat is matched: case
insensitively, with dashes and underscores treated as noise.
`deny_unknown_fields` is worth setting on every config type, since it is what
turns a misspelled option into a startup error instead of a silent difference in
behaviour. A `#[serde(alias)]` still works, because a candidate that matches no
declared field is passed through untouched for serde to resolve.

Use `ByteSize` and `Interval` from `tocat-api` for sizes and durations rather
than rolling your own. Both accept a number or a string, which matters: the
command line coerces values to integers where they parse as one, so `size=4096`
arrives as a number and `size=4k` as a string.

Validate in `build` and fail there. `block` rejects a zero size, `throttle` a
zero rate, `compress` a level outside 1 to 22, and `process` an entry giving
both `argv` and `command` or neither. All of them are startup errors, before an
endpoint is opened.

## Registration

`build` returns a `Stage`, which is either a `Filter` wrapping a `Plugin` or an
`External` describing a subprocess:

```rust,ignore
Ok(Stage::filter(Block { buf, size, flush, pad }))
```

Plugins compiled into the binary are modules of `tocat-plugins`, each behind a
cargo feature, and registered in one place:

```rust,ignore
#[cfg(feature = "block")]
mod block;

#[cfg(feature = "block")]
registry.register(block::BlockFactory);
```

Adding one is a module, a feature, and a line in `register_native`, plus a
matching feature in the `tocat` crate that forwards to it. A plugin with its own
dependency tree needs no crate of its own for that: an optional dependency
enabled by the same feature keeps it out of a build that did not ask for it,
which is how `compress` gets zstd and `wasm` gets wasmtime.

The facade exports nothing but `register_native`, so no module can reach into
another and the binary cannot reach into any of them. That is the property worth
keeping, rather than any particular arrangement of crates.
