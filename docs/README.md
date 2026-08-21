# Skuiz documentation

Skuiz builds audio plugins from one Rust type. You implement a single
[`Processor`], and the same code ships as CLAP, VST3, AUv3, or a standalone
desktop app — with a webview editor and, unusually, a built-in channel for
instances of your plugin to share state with each other.

## Start here

| If you want to | Read |
| --- | --- |
| Build your first plugin, end to end | [Getting started](getting-started.md) |
| Understand how the pieces fit | [Architecture](concepts/architecture.md) |
| Come from JUCE and map what you know | [For JUCE developers](for-juce-developers.md) |
| See it running before reading anything | `cargo run -p solid-synth --bin solid-synth-standalone` |

## Concepts

Read these in order; each assumes the one before it.

1. **[Architecture](concepts/architecture.md)** — the layer model, why one
   `Processor` reaches every format, and what each crate is for.
2. **[The Processor trait](concepts/processor.md)** — the type you implement,
   its lifecycle, and how a plugin is exported.
3. **[Threading and realtime rules](concepts/threading.md)** — which thread
   calls what, and the short list of things that must never happen on the
   audio thread. **Read this before shipping anything.**
4. **[Invariants](concepts/invariants.md)** — the ten-point realtime and
   concurrency contract the framework holds itself to, with an honest
   status per invariant.
5. **[Parameters](concepts/parameters.md)** — automation, saved state, and
   why configuration menus are just parameters with labels.
6. **[Audio bus topology](concepts/buses.md)** — declaring inputs, outputs
   and optional sidechains once, and what each host does with them.
7. **[Editors](concepts/editors.md)** — the webview model, the JavaScript
   bridge, and using a framework like SolidJS.
8. **[Instance sync](concepts/instance-sync.md)** — the part with no JUCE
   equivalent: how instances find each other, elect an owner, and share
   state across processes.

## Formats

Each format has its own packaging and its own capabilities; all are
obligation-free since the VST3 SDK went MIT in October 2025.

- **[CLAP](formats/clap.md)** — the reference target; fully working, no
  strings attached.
- **[VST3](formats/vst3.md)** — working, a default workspace member, and
  CI-checked against Steinberg's official validator.
- **[AUv3](formats/auv3.md)** — Rust side and Objective-C shim done and
  tested; needs an Xcode target you assemble.
- **[Standalone](formats/standalone.md)** — a desktop app with window,
  editor, and audio device.

## Guides

- **[Writing DSP](guides/dsp.md)** — in Rust, in C over FFI, or as an
  embedded Pure Data patch.
- **[Building and packaging](guides/packaging.md)** — bundles, formats, and
  what CI checks.

## Reference

- **API reference** — Skuiz is not yet published to crates.io, so generate
  it locally with `cargo doc --workspace --no-deps --open`.
- **[Platform support](reference/platform-support.md)** — what is tested,
  what is only type-checked, and what is missing.
- **[Troubleshooting](reference/troubleshooting.md)** — symptoms and causes.
- **[Contributing](contributing.md)** — the development workflow and the
  checks CI enforces.

## A note on honesty

This documentation distinguishes **tested**, **type-checked**, and **not
implemented**, and says which is which. Development happens on macOS; the
Windows and Linux paths compile and run in CI but have had far less
real-world exercise. Where something is unverified, the docs say so rather
than implying parity. See [platform support](reference/platform-support.md).

[`Processor`]: concepts/processor.md
