# Skuiz

A cross-platform Rust library for building audio plugin/DSP projects whose
instances communicate over IPC. Targets: CLAP, VST3, AUv3, and a standalone
shell. See `docs/` for the concepts and per-format guides.

MIT licensed. No GPL code is linked.

## Platforms

| | macOS | Windows | Linux |
| --- | --- | --- | --- |
| Audio + parameters + state | tested | type-checked | type-checked |
| Instance bus | tested (Unix socket) | type-checked (named pipe); CI only | type-checked |
| Webview editor | tested | written, unverified | not implemented |
| Standalone shell | tested | type-checked | type-checked |

**Windows and Linux are not verified at runtime.** Development happens on
macOS; the Windows paths are type-checked with
`cargo check --target x86_64-pc-windows-msvc`, and the named pipe transport
in `crates/skuiz-ipc/src/transport/windows.rs` runs only in CI's Windows
test job — it has never been exercised on real hardware, so treat it as a
starting point that needs a real test pass. Linux
compiles (its bus is the same Unix socket transport macOS uses) but the
editor has no X11 backend yet.

If you run this on Windows, `cargo test -p skuiz-ipc` is the place to start:
it covers election, promotion, broadcast, and a genuine cross-process
exchange between two processes.

## Formats and licensing

| Format | Status | Obligation on you |
| --- | --- | --- |
| CLAP | Working, validator-clean | None (MIT) |
| VST3 | Working, with editor | None — the SDK is MIT since v3.8 (Oct 2025) |
| AUv3 | Shim done and tested; needs an Xcode target | Apple developer account |
| Standalone | Working (output + test tone) | None |

`skuiz-vst3` builds on the clean-room MIT/Apache `vst3` bindings, so no
Steinberg SDK code is vendored or linked and Skuiz itself stays MIT. And
since Steinberg relicensed the VST3 SDK under MIT in v3.8 (October 2025) —
retiring the GPLv3-or-proprietary dual licence — *shipping* a VST3 binary
no longer carries a Steinberg licensing obligation either. The only
remaining string is trademark: branding a plugin with the "VST" name or
logo is optional, but doing so means following Steinberg's usage
guidelines.

```sh
cargo build                            # skuiz-vst3 is a default member
./examples/shared-gain/bundle-vst3.sh
```

## Layout

- `crates/skuiz-core` — the format-agnostic `Processor` trait
- `crates/skuiz-clap` — CLAP adapter (`export_clap!(MyProcessor)`)
- `crates/skuiz-ui` — webview editor embedding (wry / system webview)
- `crates/skuiz-ipc` — two-tier instance bus: direct in-process delivery
  plus one cross-process socket link per process, with zero-config election
  and automatic promotion
- `crates/skuiz-midi` — MIDI 1.0 and MIDI 2.0 (UMP) messages, output
  configuration
- `crates/skuiz-dsp` — embedded Pure Data via libpd (feature `libpd`)
- `crates/skuiz-vst3` — VST3 adapter (default member; see Licensing above)
- `crates/skuiz-auv3` — AUv3 C ABI, `AUAudioUnit` shim, packaging scaffold
- `crates/skuiz-standalone` — run any processor as a desktop app
- `examples/shared-gain` — gain with an IPC-shared parameter and a webview
  editor; move the slider in one instance, all instances follow
- `examples/trigger-note` — C envelope follower that fires MIDI notes, with
  dropdown configuration
- `examples/pd-tremolo` — stereo tremolo whose DSP is an embedded Pure Data
  patch (libpd, opt-in feature)
- `examples/solid-synth` — SolidJS editor driving a Rust oscillator; the
  quickest way to hear the stack work

## Instance sync

Plugin instances usually share a process — a CLAP or VST3 plugin is a
library loaded into the DAW, and every AUv3 instance inside one host shares
one extension process. So `skuiz-ipc` delivers on two tiers: instances in
the same process reach each other by direct call, while a single socket per
process (not per instance) carries traffic to instances elsewhere, such as
another DAW hosting the same plugin. Callers just use `Bus::send`.

Exactly one instance machine-wide reports `is_server()` and should own
writing shared state on save. Election is an `flock` for the owning process
(the kernel releases it on death, so it cannot go stale) plus the
longest-lived instance within it; both promote automatically when the owner
goes away.

Sync is eventually convergent: every broadcast carries a lamport version, a
late joiner pulls a snapshot of bus-edited values on join, and a reconnect
re-syncs whatever was dropped while the link was down. See
[instance sync](docs/concepts/instance-sync.md).

Because in-process delivery never touches the socket, instances inside one
host keep syncing even where the socket cannot be created at all — a
misconfigured App Group on iOS costs you cross-host sync, not everything.

## Configuration menus are just parameters

A configuration dropdown is not a separate system: a `ParamDef` with
a non-empty `choices` list is a discrete parameter, so hosts render it as an
enum, and it automates, saves with the project, and syncs over IPC like any
other parameter. `skuiz-midi::channel_param` is one ready-made example;
bit depth, scale, and microtuning selectors are written the same way.

## Hear it work

```sh
cargo run -p solid-synth --bin solid-synth-standalone
```

A SolidJS page whose signals *are* the synth's state: turn a knob or pick
a waveform and `createEffect` sends the change to a Rust oscillator. The
knobs come from [solid-knobs](https://github.com/tahti-studio/solid-knobs);
Solid and solid-knobs are vendored as prebuilt bundles
(`examples/solid-synth/src/vendor/`), so building Skuiz still needs cargo
and no JavaScript toolchain — that also shows the editor is a plain
document, not a framework lock-in.

The editor's logic has a headless check that does not need a plugin host:

```sh
npm install jsdom && node examples/solid-synth/verify-editor.mjs
```

It renders the page in a DOM and asserts that mount effects push every
parameter, that changes reach the DSP, and that values arriving from the
host are *not* echoed back — which is what stops two editors driving each
other in a loop.

## Editors

One `editor_html()` on your processor drives every format. CLAP's `gui`
extension, VST3's `IPlugView`, and the standalone window all attach the same
wry webview to a host-provided native view, so there is one editor and one
HTML file rather than one per format. VST3 additionally wraps editor changes
in `beginEdit`/`performEdit`/`endEdit`, which is what makes hosts record
automation from the GUI. macOS (NSView) is tested; Windows (HWND) is
written but unverified; Linux has no webview backend yet.

## DSP

C DSP needs no wrapper — an `extern "C"` block plus `cc` in the plugin's
`build.rs` is the whole integration (see `examples/trigger-note`). What
`skuiz-dsp` provides is Pure Data embedding, which does need help: it gives
each plugin instance its own `pdinstance` (Pd is otherwise a process-wide
singleton that would make every instance share one patch) and adapts Pd's
fixed 64-frame tick to arbitrary host block sizes.

```sh
cargo test -p skuiz-dsp --features libpd   # vendors and builds Pure Data
cargo test -p pd-tremolo --features libpd  # the example plugin that embeds it
```

`examples/pd-tremolo` shows the whole pattern: a stereo tremolo whose patch
is embedded with `include_str!`, loaded from a temp file in `activate`, and
driven from plugin parameters through `[receive]` objects.

## Building

```sh
cargo build --workspace
cargo test --workspace
./examples/shared-gain/bundle.sh   # macOS: builds target/shared-gain.clap
clap-validator validate target/shared-gain.clap
```

To start your own plugin, scaffold from an example and install it:

```sh
./tools/new-plugin.sh my-gain    # copies examples/shared-gain, rewired standalone
cd my-gain && ./install.sh       # build + install to your user CLAP dir
```

## Standalone

Any processor runs as a desktop app — window, webview editor, audio device
and a seat on the instance bus — with one call:

```sh
cargo run -p shared-gain --bin shared-gain-standalone
```

Run it *alongside* the CLAP plugin in a DAW to see the point of the whole
project: they are separate processes, so moving either gain slider drives
the other.

The shell is built on **tao + wry** — Tauri's own window and webview layers
— rather than the full Tauri framework, so the plugin and the standalone
share one webview code path and one editor contract. Tauri's bundling and
updater are packaging concerns that can wrap this later without touching
audio or editor code. Audio is output-only today, feeding the processor a
440 Hz test tone so effects are audible with no routing; capturing system
input needs a second device and drift-tolerant buffering.

To try the IPC sync in a DAW (e.g. REAPER): copy or symlink
`target/shared-gain.clap` into `~/Library/Audio/Plug-Ins/CLAP/`, load two
instances, open both editors, move one Gain slider — the other follows.
Delete the first-loaded instance and keep moving the slider: sync survives
via server promotion.

## Development

CI (`.github/workflows/ci.yml`) is switched off to keep Actions cost at
zero — the workflows stay in the tree and can be run manually from the
Actions tab, or re-enabled by restoring the push/PR triggers. When run, CI
enforces:

- `cargo fmt --all -- --check` — the tree is rustfmt-clean
- `cargo clippy --workspace --all-targets -- -D warnings` — zero warnings
- `cargo test --workspace` on **macOS, Windows and Linux** — the Windows job
  is the only place the named-pipe transport actually executes, so treat a
  Windows failure as a real finding
- the libpd integration tests (`skuiz-dsp` and `pd-tremolo --features libpd`,
  macOS)
- `cargo doc` with warnings denied — documentation links must resolve
- clap-validator over every example plugin (macOS), plus an end-to-end run
  of the scaffold tooling (`tools/new-plugin.sh` → install → validate)
- Steinberg's official VST3 `validator`, built from the MIT SDK, over the
  example `.vst3` bundle (macOS)
- the SolidJS editor's headless DOM check (Linux)

The release workflow (`.github/workflows/release.yml`, also manual-only)
builds the example bundles with `BUILD_TYPE=release` and attaches them to
a GitHub release.

Run the same locally before pushing:

```sh
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

## Deferred (add when needed)

- GPU spectral resynthesis example
- MPE per-note expression — the UMP event type (`MidiEvent`) and MIDI 2.0
  output are in; MPE note-expression events are still out
- MIDI *input* (only output ports are declared today)
- AUv3 Xcode project, provisioning and signing (the C ABI and the
  `AUAudioUnit` shim are done and tested; see crates/skuiz-auv3/scaffold)
- Standalone input capture (output-only today; the shell feeds a test tone)
  and MIDI output from the standalone (generated MIDI is currently dropped)
- Editor verification on Windows and Linux (the Linux editor embeds on X11
  via WebKitGTK; both are written but unverified)
- Running the Windows test suite on an actual Windows machine
