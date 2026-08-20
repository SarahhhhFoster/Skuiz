# CLAP

CLAP is Skuiz's reference target: fully working, validator-clean in CI,
and no licensing strings attached (MIT).

## Exporting

```rust
skuiz_clap::export_clap!(MyProcessor);
```

That one macro emits the `clap_entry` static, the plugin factory, and
the descriptor (id, name, vendor, version, features) from your
`PluginInfo`. A MIDI-emitting plugin (`emits_midi() == true`) is
advertised with the `instrument` feature; everything else is an
`audio-effect`.

## What the adapter declares

- **Audio**: one main stereo input, one main stereo output.
- **Parameters**: your full list, with choice parameters flagged
  `IS_ENUM | IS_STEPPED` and label-aware text conversion.
- **Note ports**: one MIDI 1.0 *output* port, only when `emits_midi()`.
- **State**: `save_state`/`load_state` streamed through the host's
  `clap_ostream`/`clap_istream`, with a host rescan after a load.
- **GUI**: the webview editor, on macOS (tested), Windows and Linux
  (written, unverified — Linux embeds on X11 via WebKitGTK), when
  `editor_html()` is `Some`. See [editors](../concepts/editors.md).
- **Instance sync**: the adapter joins the IPC bus at `init` and
  forwards parameter changes; no plugin code needed.

## Building and installing

A CLAP plugin is a `cdylib` plus a directory layout:

- **macOS**: `My.clap/Contents/MacOS/My` + `Contents/Info.plist`.
- **Linux**: copy the `.so` to `~/.clap/My.clap` — no bundle needed.
- **Windows**: copy the DLL to `%COMMONPROGRAMFILES%\CLAP\My.clap`.

`examples/shared-gain/bundle.sh` is a working macOS script, and
[packaging](../guides/packaging.md) walks through all three.

## Validate before you load

```sh
clap-validator validate target/shared-gain.clap
```

The validator exercises the plugin the way a host does and names
contract violations a DAW would only show you as misbehaviour. CI runs
it over every example; make it a habit before debugging a host.

## Limitations

- **Host automation is sample-accurate; IPC and editor changes are
  not** — timed events split the block, but values arriving over the
  bus carry no timestamp and apply at block top.
- **No MIDI input** — note ports are output-only; the `Processor`
  trait has no MIDI-input surface yet (deferred).
- **`reset` is a no-op** — the trait has no reset hook, so DSP state
  survives transport jumps.
- **GUI moves don't record automation gestures** — editor changes reach
  the host as a value rescan, which syncs values without a recorded
  gesture (VST3 does record; see its page).
