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

The "configuration dropdown" from PLAN.md is not a separate system.
Output channel, bit depth, scale, microtuning — each is a choice
parameter, so it automates, saves with the project, and syncs over IPC
like anything else. `skuiz_midi::channel_param(id)` is a ready-made
example: a 16-way dropdown whose labels are `"1".."16"`, read back
with `skuiz_midi::channel_of(value)`. `examples/trigger-note` uses it
for its channel and note selectors.

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
stamped at frame 41 takes effect at frame 41. (The per-parameter event
buffer is pre-allocated and bounded — a host sending more than 256
points in one block loses the excess, which is pathological anyway.)

Values from the other two paths — the editor and other instances over
IPC — carry no timestamp and apply at block top. If a control is meant
to move fast, smooth it in the DSP — see the per-sample one-pole
smoothing in `examples/solid-synth`, and note `examples/shared-gain`
skips this for brevity and says so.

## How values flow

Three paths call your `set_param`, and they are the whole story:

- **Host → DSP**: automation events, applied at the top of each block.
- **Editor → DSP**: the page posts `set_param <id> <value>`; the
  adapter applies it, tells the host to rescan, and broadcasts it to
  other instances.
- **IPC → DSP**: values from other instances park in a queue and are
  applied at block top — see [instance sync](instance-sync.md).

The reverse path — host automation reaching your editor — is the
adapter calling `window.skuizOnParam(id, value)` in the page. See
[editors](editors.md).

## Saved state

The default `save_state`/`load_state` serializes every parameter as
12-byte `(id, value)` little-endian chunks. `load_state` skips ids it
doesn't know and rejects malformed data, so states from older or newer
versions still load. Override the pair if your plugin has state beyond
parameters — and when you do, keep the tolerant-skipping behavior;
your future self shipping v2 will thank you.
