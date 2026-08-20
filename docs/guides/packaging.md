# Building and packaging

## Scaffolding: from example to installed plugin

The fastest path is to copy an example out of the repo and let the
scaffolder rewire it:

```sh
./tools/new-plugin.sh my-gain                  # template defaults to shared-gain
./tools/new-plugin.sh my-delay pd-tremolo ~/code   # any example, any parent dir
cd my-gain
./install.sh        # build + install to the per-user CLAP dir
```

`new-plugin.sh` copies the example folder, renames the crate (manifest,
library name, `src/bin/*-standalone.rs`, bundle scripts), and rewrites
`Cargo.toml` from workspace inheritance to **path dependencies on this
checkout**, so the project builds on its own. Move it to another machine
and those path deps break — switch them to
`skuiz-core = { git = "https://github.com/sarahhhh/skuiz" }` etc.

The generated `install.sh` packages the plugin and copies it where hosts
scan for the current user (`~/Library/Audio/Plug-Ins/CLAP` on macOS,
`~/.clap` on Linux; set `CLAP_INSTALL_DIR` to override). On Windows, build
with `cargo build` and copy `target/debug/my_gain.dll` to
`%COMMONPROGRAMFILES%\CLAP\my-gain.clap` yourself.

Before releasing anything, edit `src/lib.rs`: the plugin id and vendor are
still the example's, and hosts key saved projects on the id.

## The crate

A plugin is a `cdylib`:

```toml
[lib]
crate-type = ["cdylib"]
```

`export_clap!` and `export_vst3!` emit different entry-point symbols,
so one compiled library can serve both formats — the bundles below then
wrap the *same* binary. `export_auv3!` is a different shape (a static
lib linked into an Xcode extension); see [AUv3](../formats/auv3.md).

## CLAP

Build with `cargo build --release`, then place the library where hosts
scan:

- **macOS** — a bundle:

  ```sh
  BUNDLE=~/Library/Audio/Plug-Ins/CLAP/my-gain.clap
  mkdir -p "$BUNDLE/Contents/MacOS"
  cp target/release/libmy_gain.dylib "$BUNDLE/Contents/MacOS/my-gain"
  # plus Contents/Info.plist — see examples/shared-gain/bundle.sh
  ```

- **Linux** — no bundle; copy `libmy_gain.so` to `~/.clap/my-gain.clap`.
- **Windows** — copy `my_gain.dll` to
  `%COMMONPROGRAMFILES%\CLAP\my-gain.clap`.

The `Info.plist` needs `CFBundleExecutable` matching the binary name
and `CFBundleIdentifier` matching your `PluginInfo::id`. The example
scripts derive `CFBundleVersion` from the workspace version.

Then validate before loading:

```sh
clap-validator validate ~/Library/Audio/Plug-Ins/CLAP/my-gain.clap
```

## VST3

The crate is a default workspace member since the SDK went MIT in v3.8
([details](../formats/vst3.md#licensing)). The bundle is a
directory (`My.vst3/Contents/MacOS/My` on macOS);
`examples/shared-gain/bundle-vst3.sh` builds it. CI runs Steinberg's own
`validator` over the example bundle (the SDK is MIT, so CI builds it from
source); locally, test in a real host or build the validator from the SDK.

## Standalone

No packaging: `cargo run -p my-plugin --bin my-plugin-standalone`, or
`cargo build --release` and ship the binary. See
[standalone](../formats/standalone.md).

## What CI checks

CI is switched off by default (manual runs only, to keep Actions cost at
zero; re-enable the push/PR triggers in `.github/workflows/ci.yml`). When
run, it checks rustfmt, clippy with warnings
denied, the full test suite on macOS/Windows/Linux, `cargo doc` with
warnings denied, clap-validator and Steinberg's VST3 `validator` over the
example bundles, and the SolidJS editor's headless DOM check. The same
commands work locally — see [contributing](../contributing.md).

## Not covered here

Code signing and notarization for distribution, installer building, and
AUv3/Xcode packaging (which lives in `crates/skuiz-auv3/scaffold/`).
