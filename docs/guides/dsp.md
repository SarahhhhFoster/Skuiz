# Writing DSP

Your DSP lives in one method: [`Processor::process`], called on the audio
thread with the block in place. There are three ways to fill it in, in
increasing order of machinery:

1. Plain Rust — what most plugins should do.
2. C over FFI — for existing C code, with no wrapper layer.
3. Embedded Pure Data — via the optional `skuiz-dsp` crate.

Whichever you pick, the realtime rules are the same: no allocation, no
locking, no I/O, no panicking on the audio thread, and everything expensive
goes in `activate`. [Threading](../concepts/threading.md) is the full list;
read it before shipping.

[`Processor::process`]: ../concepts/processor.md

## Plain Rust

`examples/solid-synth` is the reference. The patterns it demonstrates:

**Store parameter targets, smooth in `process`.** `set_param` assigns the
raw target and nothing else; the per-sample loop ramps a smoothed copy
toward it with a one-pole, so a UI drag or an automation jump does not click
(the example has a test asserting exactly that). The smoothing coefficient
depends on the sample rate, so it is computed in `activate`.

```rust
for frame in 0..frames {
    self.freq_smoothed += self.smoothing * (freq_target - self.freq_smoothed);
    // ... use freq_smoothed, not self.freq
}
```

**Hoist transcendentals out of the sample loop.** The filter cutoff is
mapped exponentially (`20.0 * 900.0f32.powf(cutoff)`) and its coefficient
computed with `exp()` — once per block, because per-sample `powf`/`exp` is
too costly. Anything block-constant belongs above the loop.

**Allocate in `activate`, not `process`.** `activate(sample_rate,
max_frames)` is the main-thread hook where buffers, tables, and
sample-rate-dependent coefficients are built. It may be called again if the
host changes sample rate; write it to be re-entrant.

**Know your caveats.** solid-synth's waveforms are naive shapes, and the
code says so: the bright ones alias at high frequencies, and PolyBLEP is the
named fix if the plugin ever needs to sound clean rather than demonstrate a
signal path. Document yours the same way.

Also note a bus the host left inactive yields no channels — a plugin
whose output isn't connected still runs — so guard with
`outputs.main()`/`inputs.main()` rather than indexing.

## C over FFI

There is deliberately no wrapper crate for C DSP. The whole integration,
from `examples/trigger-note`:

**The C side is plain data and plain functions.** `src/envelope.h` declares
a `typedef struct` of POD fields plus two functions; `src/envelope.c`
implements a one-pole envelope follower with Schmitt-trigger thresholding.

**Rust mirrors the struct with `#[repr(C)]` and declares the functions in an
`extern "C"` block** (`examples/trigger-note/src/lib.rs`):

```rust
/// Mirrors `skuiz_env` in envelope.h. Plain data, so C owns the layout
/// and Rust just hands over a pointer.
#[repr(C)]
#[derive(Default)]
struct CEnv {
    env: f32,
    attack: f32,
    release: f32,
    open: i32,
}

extern "C" {
    fn skuiz_env_init(e: *mut CEnv, sample_rate: f32);
    fn skuiz_env_scan(
        e: *mut CEnv, samples: *const f32, frames: i32,
        threshold: f32, out_closed: *mut i32,
    ) -> i32;
}
```

**`build.rs` compiles the C with the `cc` crate** (a `build-dependencies`
entry, nothing more):

```rust
fn main() {
    println!("cargo:rerun-if-changed=src/envelope.c");
    println!("cargo:rerun-if-changed=src/envelope.h");
    cc::Build::new().file("src/envelope.c").compile("envelope");
}
```

Three rules make this stay sane:

- **Keep C state in your struct, not in globals.** `TriggerNote` owns its
  `CEnv` as a field, so every plugin instance gets its own follower. A
  `static` in the C file would be shared by every instance in the process —
  in a DAW that is wrong by construction.
- **Confine `unsafe` to the call sites.** The `extern` calls are the only
  unsafe code; everything around them is safe Rust.
- **Initialize in `activate`.** `skuiz_env_init` takes the sample rate, so
  it is called from `activate`, never lazily in `process`.

trigger-note also shows MIDI output and configuration dropdowns. Choice
parameters — a `ParamDef` with a non-empty `choices` list, such as the note
picker — render as dropdowns and automate like anything else (see
[parameters](../concepts/parameters.md)). Emitting MIDI is three pieces:
`fn emits_midi() -> bool { true }`, messages built with `skuiz_midi`
helpers (`note_on`, `note_off`), and `midi.push(frame,
bytes)` inside `process`, where `frame` is the offset within the block.
`MidiOut` is bounded (512 events) and never allocates; when the buffer is
full, `push` refuses the event and the adapter counts it in the
`midi_events_dropped` diagnostic (see
[invariants](../concepts/invariants.md)) — which means the DSP is emitting
far too many events per block, a bug to fix there. The example's `tests/midi_out.rs` drives the raw CLAP vtable and
asserts the exact bytes on the wire (`[0x90, 60, 100]` and back off), which
is the way to test this without a host.

## Embedded Pure Data

`skuiz-dsp` embeds Pure Data through libpd, behind the `libpd` feature,
which is **off by default** — enabling it vendors and compiles Pure Data:

```sh
cargo test -p skuiz-dsp --features libpd
```

The crate exists because embedding Pd in a plugin has two sharp edges, and
`PdEngine` handles both:

- **Pd is a process-wide singleton by default** — two plugin instances
  would share one patch and one set of receivers. Each `PdEngine` owns a
  separate `pdinstance` and selects it before every call, so instances stay
  independent (there is a test proving one engine's patch does not leak into
  another's output).
- **Pd only processes 64 frames at a time**, while hosts hand over any
  block size — 100 frames, or a different size every block.
  `PdEngine::process` adapts between the two with an internal ring buffer,
  at the cost of a constant `PdEngine::latency_frames()` samples of delay
  (one 64-frame tick). Return it from `Processor::latency` — the CLAP and
  VST3 adapters report it to the host, so the DAW can delay-compensate.

The API, all of it:

```rust
// Off the audio thread — allocates and takes the global Pd setup lock.
// `channels` is used for both input and output.
let mut pd = PdEngine::new(48_000.0, 2)?;

// Off the audio thread as well.
pd.open_patch(Path::new("my-patch.pd"))?;

// Drive [receive] objects from your parameters.
pd.send_float("cutoff", 0.7);

// In Processor::process: any block size, realtime-safe. Takes the flat
// in-place channel array — hand it `outputs.main()`'s channels, which the
// adapter already copied the input into.
pd.process(main.channels());
```

The split matters: `new` and `open_patch` allocate and touch process-wide Pd
state, so they belong in `activate` (or instance setup); `process` is
lock-free and allocation-free. Drop frees the instance and stops its DSP.

`examples/pd-tremolo` is the working example: a stereo tremolo whose patch
is embedded with `include_str!`, written to a temp file and loaded in
`activate`, and driven from plugin parameters through `[receive]` objects —
see its `tests/clap.rs` for driving it through the raw CLAP vtable.

## Where to go next

- [Threading and realtime rules](../concepts/threading.md) — the
  correctness page.
- [Parameters](../concepts/parameters.md) — ranges, choice lists, saved
  state.
- `examples/trigger-note`, `examples/solid-synth`, `examples/pd-tremolo` —
  the code this page summarizes.
