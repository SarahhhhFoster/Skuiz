# The Processor trait

Everything a Skuiz plugin does lives in one trait, defined in
`skuiz-core`. The adapters translate host callbacks into calls on it;
your code never mentions a format. This page walks the contract method
by method — see `cargo doc -p skuiz-core` for signatures.

## Lifecycle

A host drives your type in a fixed order:

1. **`Default::default()`** — the instance's initial state. Skuiz
   constructs instances with `Default`, so put your defaults here.
2. **Bus join** — the adapter seats the instance on the
   [instance-sync](instance-sync.md) bus before any audio runs.
3. **`activate(sample_rate, max_frames)`** — the one place to allocate
   buffers, build tables, and reset state. Called again if the sample
   rate changes.
4. **`process(...)`** — called per block, on the audio thread, until
   playback stops.
5. **`deactivate()`** — release what `activate` set up.
6. **Destroy** — the instance is dropped.

## The methods

**`info()`** returns static metadata. `id` is reverse-DNS and must be
globally unique and immutable once released: hosts key saved projects
on it, the VST3 class id is derived from it, and instances find each
other on the bus by it. Two plugins sharing an id share a bus.

**`params()`** returns every parameter as a `&'static [ParamDef]`, in
display order. The list is static — hosts snapshot it at load time, so
parameters cannot appear or disappear at runtime. See
[parameters](parameters.md).

**`set_param(id, value)` / `get_param(id)`** apply and read parameter
changes. While blocks flow, `set_param` always runs on the audio thread —
automation, IPC, and editor changes all reach it there — and on the main
thread when the engine is stopped (state load, direct edits), so keep it
to arithmetic and assignment, and clamp — hosts are not obliged to
respect your declared range.

**`process(channels, midi)`** renders one block in place: each channel
slice arrives holding input and must leave holding output. All slices
are the same length, which varies between calls. `channels` may be
empty — a MIDI-only plugin still gets called. Push generated MIDI into
`midi` (which arrives cleared); it has a fixed capacity of 512 events,
never allocates, and refuses events once full — the refusal is counted in
the `midi_events_dropped` diagnostic, never silent, and a full buffer
means the DSP is emitting far too many events per block, which is a bug
in the DSP. This method
is realtime: read [threading](threading.md) before shipping.

**`reset()`** (default no-op) clears DSP state — delay lines, filter
memory, LFO phase — without touching parameter values. Called between
blocks on the audio thread while running, on the main thread when
stopped. AUv3 hosts and the CLAP adapter call it on host resets; VST3
has no reset concept, so there it only fires if you call
`Engine::reset` yourself.

**`latency()`** (default 0) reports the plugin's delay in frames. It may
change at runtime: the engine re-reads it once per block and, on change,
updates the value hosts see and notifies them (CLAP and VST3; AUv3 and
standalone report no change notification). Because of that poll it runs
on the audio thread — keep it to reading a field.

**`emits_midi()`** (default `false`) tells adapters whether to
advertise a note output port, so an audio-only plugin doesn't show
hosts a MIDI out that never fires.

**`editor_html()` / `editor_size()`** — the editor is a static HTML
string (usually `include_str!`) plus a size in logical pixels. `None`
(the default) means no GUI. See [editors](editors.md).

**`save_state()` / `load_state(data)`** — the default implementation
writes a `SKZ1` version header followed by every parameter as 12-byte
`(id: u32 LE, value: f64 LE)` chunks; `load_state` skips unknown ids,
so states from other versions still load, accepts the legacy
headerless format from pre-versioning builds, and returns `false` for
malformed data. Override both if you have non-parameter state — and
version your own format with a header the same way.

## Exporting

One line per format, in your `cdylib` crate:

```rust
skuiz_clap::export_clap!(MyProcessor);
skuiz_vst3::export_vst3!(MyProcessor); // needs the skuiz-vst3 dependency
```

The macros emit different entry-point symbols, so both can live in one
binary. The `Processor` implementation itself contains no format
conditionals — feature-gating the *macro invocation* (as
`examples/shared-gain` does with its `vst3` feature) is a packaging
choice, not a code fork. See the format pages under
[formats](../formats/clap.md) for each adapter's specifics.
