# Audio bus topology

Which audio buses a plugin has — inputs, outputs, their channel layouts —
is **declared once, at build time**, as static metadata. Hosts may activate
or deactivate an *optional* bus, but no bus is ever created or destroyed at
runtime, and the declaration never changes while the plugin is loaded.

## Declaring

Like `params()`, the topology is a `&'static` slice on the `Processor`
trait:

```rust
use skuiz_core::{AudioBusSpec, ChannelLayout};

impl Processor for SidechainComp {
    fn audio_buses() -> &'static [AudioBusSpec] {
        &[
            AudioBusSpec::input("Main", ChannelLayout::Stereo),
            AudioBusSpec::input("Sidechain", ChannelLayout::Mono).optional(),
            AudioBusSpec::output("Main", ChannelLayout::Stereo),
        ]
    }
    // ...
}
```

Each `AudioBusSpec` carries a stable [`BusId`] (a const FNV-1a hash of the
name — pin it with `.with_id(n)` if you ever rename a bus), a display name,
a direction, a `ChannelLayout` (`Mono`, `Stereo`, or `Discrete(n)` up to 8
channels; named surround layouts are deliberately deferred), and whether it
is `optional`.

Two ready-made topologies cover the common cases:

- `skuiz_core::bus::DEFAULT_EFFECT_BUSES` — stereo main in, stereo main
  out. This is the **default**: an effect declares nothing.
- `skuiz_core::bus::INSTRUMENT_BUSES` — no inputs, stereo main out.

The rules (`validate_buses`, debug-asserted at engine construction): ids
are unique per direction, the first bus of a direction is the *main* bus
and cannot be optional, at most 4 buses per direction.

## Reading buses in `process`

```rust
fn process(&mut self, inputs: &AudioInputs, outputs: &mut AudioOutputs, midi: &mut MidiOut) {
    let Some(main_in) = inputs.main() else { return };      // None: instrument
    let Some(main_out) = outputs.main() else { return };
    let side = inputs
        .get(BusId::from_name("Sidechain"))
        .and_then(|b| b.channel(0));   // None when inactive

    for (ic, oc) in main_in.channels().iter().zip(main_out.channels()) {
        for (i, o) in ic.iter().zip(oc.iter_mut()) {
            let gain = if side.is_some() { 0.5 } else { 1.0 };
            *o = *i * gain;
        }
    }
}
```

- **Check `active()`, or just index.** An optional bus the host didn't
  connect reports `active() == false` and yields no channels — `channel(0)`
  returns `None`, so guarded access handles it without a branch.
- **The main input may alias the main output.** Hosts process in place:
  after the adapter's copy-in, both views can point at the same memory.
  Read a frame before writing it and this is invisible.
- **Lookup is free.** Views are built on the stack each block over
  preallocated engine scratch; `get(BusId)` is a tiny linear scan over
  static declarations. No allocation, no locking, no string lookup on the
  audio thread — the [invariants](invariants.md) apply here as everywhere.

## What each host sees

The adapters translate the same declaration into their native model; the
processor never sees host bus concepts.

| Format | Translation |
| --- | --- |
| CLAP | One audio port per bus: `CLAP_PORT_MONO`/`STEREO` port type, `IS_MAIN` on the main bus, `in_place_pair` linking the main pair. An unconnected optional port arrives inactive. |
| VST3 | One audio bus per spec: `kMain` for the main bus, `kAux` for sidechain inputs, `kDefaultActive` only on non-optional buses. `activateBus` toggles optional buses; `setBusArrangements` validates against the declared layouts (`kEmpty` allowed only for a deactivated optional bus). |
| AUv3 | The shim builds `inputBusses`/`outputBusses` from the declaration; sidechain buses are pulled only when the host connected them, and a failed pull means inactive, not an error. |
| Standalone | No negotiation: the main buses run (tone/silence feeds the main input), declared sidechains stay inactive. |

## Not here, on purpose

Dynamic bus creation, automatic up/downmixing, routing graphs, MIDI
topology (see [processor](processor.md) for `emits_midi`), and editor
routing UI are all out of scope. If the host sends more channels than a
bus declares, the adapter clamps to the declaration.

[`BusId`]: ../../crates/skuiz-core/src/bus.rs
