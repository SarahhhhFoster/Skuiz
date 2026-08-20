# Contributing

## Layout

- `crates/skuiz-core` — the `Processor` trait and shared contracts; no
  format code.
- `crates/skuiz-{clap,vst3,auv3,standalone}` — the adapters; the only
  code that knows what a host is.
- `crates/skuiz-{ui,ipc,midi,dsp}` — services the adapters wire in.
- `examples/` — three complete plugins; every change should keep them
  working and CI-green.

`skuiz-vst3` is an ordinary default member: it was excluded while
shipping a VST3 binary carried a Steinberg licensing obligation, which
ended when the SDK went MIT in v3.8 (October 2025).

## Before you push

```sh
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

CI (`.github/workflows/ci.yml`) is switched off to keep Actions cost at
zero — it runs manually from the Actions tab, or re-enable the push/PR
triggers in the workflow file. When run, it enforces:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace` on macOS, Windows, and Linux — the Windows
  job is the only place the named-pipe transport executes, so treat a
  failure there as real
- the libpd integration test on macOS
- `cargo doc --workspace --no-deps` with warnings denied — doc links
  must resolve
- clap-validator over every example plugin (macOS), plus an end-to-end run
  of the scaffold tooling: copy an example, build it standalone, install,
  validate
- Steinberg's official VST3 `validator`, built from the MIT SDK, over the
  example `.vst3` bundle (macOS)
- the SolidJS editor's headless DOM check (Linux)

The release workflow (`.github/workflows/release.yml`, also manual-only)
builds the example bundles in release mode and attaches them to a GitHub
release.

## Conventions

- **Licensing purity is a requirement.** Skuiz is MIT and links no GPL
  code. Do not vendor or copy from JUCE — the VST3 side uses the
  clean-room `vst3` bindings for exactly this reason. (The Steinberg
  SDK itself is MIT since v3.8, so consulting or vendoring it is no
  longer a GPL concern — but keep the clean-room bindings anyway; they
  are why nothing here depends on Steinberg's C++.)
- **Docs are honest about verification.** Say *tested*, *type-checked*,
  or *not implemented* — never imply parity. If you change what is
  verified, update [platform support](reference/platform-support.md)
  and the README's tables and Deferred list in the same change.
- **Doc comments are terse** and state threading and realtime rules at
  the point of use. Known limitations are marked `ponytail` in code and
  listed in the README's Deferred section — keep the two in sync.
- **Examples are teaching material.** Comments explain the why,
  including deliberate omissions.

## Adding a format adapter

Read `crates/skuiz-clap` first — it is the reference. An adapter owns:
instance lifecycle (`Default` → bus join → `activate` → `process` →
`deactivate`), host parameter reporting, state streaming, editor
attachment via `skuiz-ui`, and panic containment at the FFI boundary
(the `ffi_guard` pattern). The threading contract in
[threading](concepts/threading.md) is the spec you are implementing.
