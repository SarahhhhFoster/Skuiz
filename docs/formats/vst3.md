# VST3

VST3 works — with the editor — but is opt-in, because shipping a VST3
binary carries an obligation CLAP does not.

## Licensing

`skuiz-vst3` builds on the clean-room MIT/Apache `vst3` bindings crate:
no Steinberg SDK code is vendored or linked, and Skuiz itself stays
MIT. But **shipping a VST3 binary** is licensed by Steinberg under
either GPLv3 or their separate free-of-charge proprietary agreement.
That obligation is why `skuiz-vst3` is deliberately excluded from the
workspace's default members — building it is a choice you make:

```sh
cargo build -p skuiz-vst3
./examples/shared-gain/bundle-vst3.sh
```

`examples/shared-gain` shows the pattern: `skuiz-vst3` as an optional
dependency, and the export behind a feature:

```rust
#[cfg(feature = "vst3")]
skuiz_vst3::export_vst3!(SharedGain);
```

The macro coexists with `export_clap!` in one `cdylib` — different
entry-point symbols, same compiled library inside both bundles.

## Design notes a shipper should know

- **The class id is derived, not hand-minted.** A const FNV-1a hash of
  your `PluginInfo::id`, folded to 16 bytes. Stable across builds — and
  one more reason the plugin id must never change.
- **Single component.** One object implements `IComponent`,
  `IAudioProcessor`, and `IEditController`, so processor and controller
  can never disagree about parameter state.
- **GUI edits record automation.** Editor changes are wrapped in
  `beginEdit`/`performEdit`/`endEdit`, which is what makes hosts record
  automation from the GUI — the CLAP adapter does not do this.
- **Parameters are normalized** 0..1 at the boundary, with `stepCount`
  and `kIsList` for choice parameters.
- **Editor**: the same wry webview as everywhere else, via `IPlugView`.
  macOS tested; Windows written, unverified; none on Linux.

## Limitations

- **Note on/off only from `MidiOut`.** CC and pitch-bend bytes are
  dropped at the event conversion (a documented ponytail — the event
  type is narrower than VST3's).
- **No MIDI input** — same trait-level gap as CLAP.
- MIDI event frame offsets are clamped into the block; DSP that
  timestamps past the block end loses that timing.

## Validating

There is no VST3 equivalent of clap-validator in-tree. The COM contract
is covered by `cargo test -p skuiz-vst3` (a test drives the factory,
processing, state, and edit gestures the way a host would); beyond
that, test in a real host. See
[platform support](../reference/platform-support.md) for the honesty
matrix.
