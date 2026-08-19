# Contributing

## Layout

- `crates/skuiz-core` — the `Processor` trait and shared contracts; no
  format code.
- `crates/skuiz-{clap,vst3,auv3,standalone}` — the adapters; the only
  code that knows what a host is.
- `crates/skuiz-{ui,ipc,midi,dsp}` — services the adapters wire in.
- `examples/` — three complete plugins; every change should keep them
  working and CI-green.

`skuiz-vst3` is deliberately **not** a workspace default member:
shipping a VST3 binary carries a Steinberg licensing obligation, so
building it is an explicit choice. `cargo test --workspace` still
includes it.

## Before you push

```sh
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

CI (`.github/workflows/ci.yml`) runs on every push to `main` and every
pull request, and enforces:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace` on macOS, Windows, and Linux — the Windows
  job is the only place the named-pipe transport executes, so treat a
  failure there as real
- the libpd integration test on macOS
- `cargo doc --workspace --no-deps` with warnings denied — doc links
  must resolve
- clap-validator over every example plugin (macOS)
- the SolidJS editor's headless DOM check (Linux)

## Conventions

- **Licensing purity is a requirement.** Skuiz is MIT and links no GPL
  code. Do not vendor or copy from JUCE or the Steinberg SDK — the VST3
  side uses the clean-room `vst3` bindings for exactly this reason.
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
