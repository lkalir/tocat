# Pipeline construction

A run has two endpoint slots and an ordered list of pipeline entries. Turning
that into two chains of stage instances is done by the registry, and it is where
nearly all the validation lives.

## Slots and positions

Roles are decided by position, never by looking at the text. `--from` and `--to`
fill the first and last slots; the outer positional arguments fill whichever of
those are still open, outermost first; whatever remains in the middle is the
pipeline. A lone positional with both slots open fills the source.

```
tocat SRC SINK                    -> no plugins
tocat SRC tee compress SINK       -> two entries, in that order
tocat -f SRC tee SINK             -> one entry, SINK fills the open slot
tocat -f SRC -t SINK tee          -> one entry, both slots already filled
```

An endpoint and a plugin are written the same way, so guessing between them from
the text would turn a typo into a different program, and would make the meaning
of a command line depend on which plugins the binary was built with. The rule is
worth keeping strictly.

## From entries to chains

Each entry names a plugin, optionally a direction, and a set of options, of
which `as` and `detach` are consumed by the host and the rest passed through
untouched as JSON for the plugin's own `Deserialize`. That is what lets the host
stay ignorant of plugin schemas, including, eventually, those of plugins it has
never seen.

Both chains are built from the same declaration list. Direction decides which
one an entry joins:

| Direction        | Forward chain | Reverse chain |
| ---------------- | ------------- | ------------- |
| `source-to-sink` | one instance  |               |
| `sink-to-source` |               | one instance  |
| `both`           | one instance  | one instance  |

`both` builds two independent instances rather than sharing one, so
per-direction state never leaks across paths. Under `fork` the whole
construction is repeated per connection, so an instance is scoped to one path of
one connection and never needs a lock.

## Order

The forward chain is the surviving entries in the order written; the reverse
chain is the same list reversed. That is what makes the command line a picture
of the wire:

```
tocat SRC  a  b  c  SINK

source -> sink:   SRC -> a -> b -> c -> SINK
sink -> source:   SRC <- a <- b <- c <- SINK
```

Mirroring rather than repeating is what makes wrapping stages nest correctly.
Declaring compression nearer the source and framing nearer the sink gives, on
the way back, unframing before decompression, with no second declaration and no
way for the two directions to disagree.

Config-file entries come first, then the inline positional pipeline, then `-p`,
so an ad-hoc stage lands nearest the sink on the forward path.

## Naming

Each instance gets a display name: the `as` alias if given, otherwise the plugin
name, with `#1`, `#2` appended only where that name would appear more than once
on the same path. It is what logs are tagged with, what `tee` puts in its
header, and what a stage sees as `StageInfo::name`.

Each instance is also told its neighbours on this path, which are the adjacent
stages' display names or an endpoint label at either end. A stage wedged between
two others therefore describes the hop it is really watching rather than the
endpoints it is nowhere near. Under `fork` the accepted peer is folded into the
listening endpoint's label first, so those descriptions identify the connection
too.

## Segmentation

While building, stages accumulate into a draft segment. A stage whose placement
resolves to `Detached` closes the draft and starts a new one, and an external
stage closes the draft, becomes a segment of its own, and forces the next inline
stage to start another. Placement is the entry's `detach` if it gave one,
otherwise the factory's default.

## What is decided here

- Which plugins exist in this binary, so a name the build does not have is a
  startup error, with a suggestion drawn from the ones it does have.
- Whether every option belongs to the plugin that documents it, and whether the
  values parse.
- Display names, including the `#n` suffixes.
- Placement, including rejecting `detach = false` on a stage that runs as a
  subprocess.
- Each stage's tick interval, read once, so a segment with nothing ticking
  builds no timer.
- Which side channels exist, de-duplicated by target across both directions.
- Whether any stage on a path may break message boundaries, which produces the
  [datagram warning](datagrams.md).

Running with `-v` logs the resolved chains, their segment counts and the number
of channels, which is the intended way to check that a long command line came
out as intended.
