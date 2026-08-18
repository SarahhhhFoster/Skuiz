# Skuiz

A cross-platform Rust library for building audio plugin/DSP projects whose
instances communicate over IPC. Targets: CLAP today; VST3, AUv3, and a
standalone shell as the adapters land. See `PLAN.md` for the architecture.

MIT licensed. No GPL code is linked.

## Platforms

| | macOS | Windows | Linux |
| --- | --- | --- | --- |
| Audio + parameters + state | tested | type-checked | type-checked |
| Instance bus | tested (Unix socket) | type-checked (named pipe) | type-checked |
| Webview editor | tested | written, unverified | not implemented |
| Standalone shell | tested | type-checked | type-checked |

**Windows and Linux are not verified at runtime.** Development happens on
macOS; the Windows paths are type-checked with
`cargo check --target x86_64-pc-windows-msvc` and nothing more. The named
pipe transport in `crates/skuiz-ipc/src/transport/windows.rs` has never been
executed — treat it as a starting point that needs a real test pass. Linux
compiles (its bus is the same Unix socket transport macOS uses) but the
editor has no X11 backend yet.

If you run this on Windows, `cargo test -p skuiz-ipc` is the place to start:
it covers election, promotion, broadcast, and a genuine cross-process
exchange between two processes.

## Formats and licensing

| Format | Status | Obligation on you |
| --- | --- | --- |
| CLAP | Working, validator-clean | None (MIT) |
| VST3 | Working, with editor | Steinberg licence to **ship** a binary |
| AUv3 | Shim done and tested; needs an Xcode target | Apple developer account |
| Standalone | Working (output + test tone) | None |

`skuiz-vst3` builds on the clean-room MIT/Apache `vst3` bindings, so no
Steinberg SDK code is vendored or linked and Skuiz itself stays MIT. But
*shipping* a VST3 binary is licensed by Steinberg under either GPLv3 or
their separate free-of-charge proprietary agreement, so the crate is
excluded from the workspace's default members — building it is a deliberate
choice, not something `cargo build` does for you.

```sh
cargo build -p skuiz-vst3          # explicit
./examples/shared-gain/bundle-vst3.sh
```

## Layout

- `crates/skuiz-core` — the format-agnostic `Processor` trait
- `crates/skuiz-clap` — CLAP adapter (`export_clap!(MyProcessor)`)
- `crates/skuiz-ui` — webview editor embedding (wry / system webview)
- `crates/skuiz-ipc` — two-tier instance bus: direct in-process delivery
  plus one cross-process socket link per process, with zero-config election
  and automatic promotion
- `crates/skuiz-midi` — MIDI 1.0 messages and output configuration
- `crates/skuiz-dsp` — embedded Pure Data via libpd (feature `libpd`)
- `crates/skuiz-vst3` — VST3 adapter (opt-in; see Licensing below)
- `crates/skuiz-auv3` — AUv3 C ABI, `AUAudioUnit` shim, packaging scaffold
- `crates/skuiz-standalone` — run any processor as a desktop app
- `examples/shared-gain` — gain with an IPC-shared parameter and a webview
  editor; move the slider in one instance, all instances follow
- `examples/trigger-note` — C envelope follower that fires MIDI notes, with
  dropdown configuration
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

Because in-process delivery never touches the socket, instances inside one
host keep syncing even where the socket cannot be created at all — a
misconfigured App Group on iOS costs you cross-host sync, not everything.

## Configuration menus are just parameters

PLAN.md's configuration dropdown is not a separate system: a `ParamDef` with
a non-empty `choices` list is a discrete parameter, so hosts render it as an
enum, and it automates, saves with the project, and syncs over IPC like any
other parameter. `skuiz-midi::channel_param` is one ready-made example;
bit depth, scale, and microtuning selectors are written the same way.

## Hear it work

```sh
cargo run -p solid-synth --bin solid-synth-standalone
```

A SolidJS page whose signals *are* the synth's state: move a slider or pick
a waveform and `createEffect` sends the change to a Rust oscillator. Solid is
vendored as a prebuilt 30 KB bundle (`examples/solid-synth/src/vendor/`), so
building Skuiz still needs cargo and no JavaScript toolchain — that also
shows the editor is a plain document, not a framework lock-in.

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
automation from the GUI. macOS (NSView) only for now.

## DSP

C DSP needs no wrapper — an `extern "C"` block plus `cc` in the plugin's
`build.rs` is the whole integration (see `examples/trigger-note`). What
`skuiz-dsp` provides is Pure Data embedding, which does need help: it gives
each plugin instance its own `pdinstance` (Pd is otherwise a process-wide
singleton that would make every instance share one patch) and adapts Pd's
fixed 64-frame tick to arbitrary host block sizes.

```sh
cargo test -p skuiz-dsp --features libpd   # vendors and builds Pure Data
```

## Building

```sh
cargo build --workspace
cargo test --workspace
./examples/shared-gain/bundle.sh   # macOS: builds target/shared-gain.clap
clap-validator validate target/shared-gain.clap
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

CI (`.github/workflows/ci.yml`) enforces on every push and pull request:

- `cargo fmt --all -- --check` — the tree is rustfmt-clean
- `cargo clippy --workspace --all-targets -- -D warnings` — zero warnings
- `cargo test --workspace` on **macOS, Windows and Linux** — the Windows job
  is the only place the named-pipe transport actually executes, so treat a
  Windows failure as a real finding
- `cargo doc` with warnings denied — documentation links must resolve
- clap-validator over every example plugin (macOS)
- the SolidJS editor's headless DOM check (Linux)

Run the same locally before pushing:

```sh
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings   && cargo test --workspace
```

## Deferred (add when needed)

- Sample-accurate parameter automation (events are block-quantized)
- Lock-free parameter sync (currently a Mutex around the processor)
- GPU spectral resynthesis example; a plugin example that embeds libpd
  (the engine and its tests exist, no example plugin uses it yet)
- MPE / MIDI 2.0 UMP output — both need an event wider than the 3 bytes
  `MidiOut` carries, so they land together with a wider event type
- MIDI *input* (only output ports are declared today)
- AUv3 Xcode project, provisioning and signing (the C ABI and the
  `AUAudioUnit` shim are done and tested; see crates/skuiz-auv3/scaffold)
- VST3 CC / pitch-bend output (note on/off are converted today)
- Standalone input capture (output-only today; the shell feeds a test tone)
  and MIDI output from the standalone (generated MIDI is currently dropped)
- Linux webview editor (X11); Windows editor is written but unverified
- Running the Windows test suite on an actual Windows machine
