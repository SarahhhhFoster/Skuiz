# VST3

VST3 works — with the editor — and is an ordinary default workspace
member since the licensing question went away (see below).

## Licensing

`skuiz-vst3` builds on the clean-room MIT/Apache `vst3` bindings crate:
no Steinberg SDK code is vendored or linked, and Skuiz itself stays MIT.
And since Steinberg relicensed the VST3 SDK under MIT in v3.8.0
(2025-10-20) — retiring the GPLv3-or-proprietary dual licence —
**shipping a VST3 binary carries no Steinberg licensing obligation**:
no fees, no paperwork, commercial and closed-source shipping allowed.
The only remaining condition is trademark: using the "VST" name or logo
is optional, but if you do, Steinberg's VST usage guidelines (included
in the SDK) are mandatory.

The crate used to be excluded from the workspace's default members over
that obligation; with it gone, `skuiz-vst3` builds and tests like every
other crate:

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
- **Buses come from your declaration.** Each declared audio bus becomes
  a VST3 bus: `kMain` for the main pair, `kAux` for sidechain inputs,
  `kDefaultActive` only on non-optional buses, and speaker arrangements
  validated against the declared layouts in `setBusArrangements`. See
  [buses](../concepts/buses.md).
- **GUI edits record automation.** Editor changes are wrapped in
  `beginEdit`/`performEdit`/`endEdit`, which is what makes hosts record
  automation from the GUI — the CLAP adapter does not do this.
- **Parameters are normalized** 0..1 at the boundary, with `stepCount`
  and `kIsList` for choice parameters.
- **Editor**: the same wry webview as everywhere else, via `IPlugView`.
  macOS tested; Windows written, unverified; Linux written, unverified
  (X11 embed via WebKitGTK).

## Limitations

- **MIDI 1.0-reducible events only from `MidiOut`.** Note on/off and poly
  pressure become native VST3 events; CC, pitch bend and channel pressure
  become `kLegacyMIDICCOutEvent`. UMP-only MIDI 2.0 events are skipped —
  VST3 has no UMP event type (a documented ponytail).
- **No MIDI input** — same trait-level gap as CLAP.
- MIDI event frame offsets are clamped into the block; DSP that
  timestamps past the block end loses that timing.
- **Remote changes reach the host, not the open editor.** When a
  parameter moves via the bus or a state load while blocks flow, the
  host is told with `restartComponent(kParamValuesChanged)` at the next
  `getParamNormalized`, so its automation lanes and generic editor
  converge. The webview editor is *not* pushed a refresh: VST3 has no
  equivalent of CLAP's `request_callback`, and a webview `eval` is only
  legal on the thread that created it (a documented ponytail — a UI
  timer would close it).

## Validating

CI runs Steinberg's official `validator` — built from the MIT SDK — over
the example bundle (`.github/workflows/ci.yml`, job `vst3-validation`;
CI runs on pushes to `main` and on pull requests, and can be dispatched
manually — only the release workflow is manual-only). The COM contract is also
covered by
`cargo test -p skuiz-vst3` (a test drives the factory, processing, state,
and edit gestures the way a host would); beyond that, test in a real
host. See [platform support](../reference/platform-support.md) for the
honesty matrix.
