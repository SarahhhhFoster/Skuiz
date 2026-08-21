# Parameters

A parameter is a `ParamDef`:

```rust
ParamDef {
    id: 0,                 // stable forever — saved projects refer to it
    name: "Gain",
    min: 0.0,              // continuous range...
    max: 1.0,
    default: 1.0,
    choices: &[],          // ...or a non-empty list for a discrete one
    shared: true,          // editor moves sync across instances; false = local only
}
```

## Continuous vs choice

A parameter with an empty `choices` list is continuous over
`min..=max`. A parameter with a non-empty list is **discrete**: its
legal values are the indices `0..choices.len()`, `min`/`max` are
ignored, and hosts render it as a stepped enum — CLAP gets
`IS_ENUM | IS_STEPPED`, VST3 gets `stepCount` + `kIsList`, AUv3 gets an
indexed parameter with `valueStrings`.

Use `def.low()` / `def.high()` rather than reading `min`/`max`
directly — they return the effective range for both kinds.
`def.label(value)` maps a value back to its choice label, returning
`None` for out-of-range values so a wrong value shows up as a number
instead of a plausible lie.

## Configuration menus are just parameters

A configuration dropdown is not a separate system.
Output channel, bit depth, scale, microtuning — each is a choice
parameter, so it automates, saves with the project, and syncs over IPC
like anything else. `skuiz_midi::channel_param(id)` is a ready-made
example: a 16-way dropdown whose labels are `"1".."16"`, read back
with `skuiz_midi::channel_of(value)`. `examples/trigger-note` uses it
for its channel selector (its note selector is a hand-rolled choice
parameter).

## Rules that keep saved projects working

- **Ids are forever.** Changing a parameter's id (or your plugin id)
  orphans saved values. Add new parameters; never renumber old ones.
- **Defaults matter.** `default` is what a fresh instance starts at and
  what hosts reset to.
- **Clamp in `set_param`.** Hosts are not obliged to respect your
  declared range, and neither are IPC peers.

## Automation timing

Host automation is **sample-accurate**: the adapters split each block
at parameter-event times and render the segments in order, so a change
stamped at frame 41 takes effect at frame 41. (The per-block event
buffer is pre-allocated and bounded — 256 events per block, shared
across all parameters; a host sending more than that in one block loses
the excess, which is pathological anyway.)

Values from the other two paths — the editor and other instances over
IPC — carry no timestamp and apply at block top while the transport
runs (they apply directly when the engine is stopped). If a control is meant
to move fast, smooth it in the DSP — see the per-sample one-pole
smoothing in `examples/solid-synth`, and note `examples/shared-gain`
skips this for brevity and says so.

## How values flow

Three paths call your `set_param`, and they are the whole story:

- **Host → DSP**: automation events, applied sample-accurately — the
  adapter splits the block at event times, as described above.
- **Editor → DSP**: the page posts `set_param <id> <value>`; the
  adapter applies it, tells the host to rescan, and — for parameters
  declared `shared` — broadcasts it to other instances.
- **IPC → DSP**: values from other instances park in a queue and are
  applied at block top (or directly, when the engine is stopped) — see
  [instance sync](instance-sync.md). Frames
  naming a `shared: false` parameter are ignored, not applied.

The reverse path — remote and state-load changes reaching your editor —
is the adapter calling `window.skuizOnParam(id, value)` in the page.
Host automation never reaches the editor. See
[editors](editors.md).

## Saved state

The default `save_state`/`load_state` writes a `SKZ1` version header
followed by every parameter as 12-byte `(id, value)` little-endian
chunks. `load_state` skips ids it doesn't know, accepts the legacy
headerless format from pre-versioning builds, and rejects malformed
data, so states from older or newer versions still load. Override the
pair if your plugin has state beyond parameters — and when you do,
keep the tolerant-skipping behavior and version your own format with a
header; your future self shipping v2 will thank you.
