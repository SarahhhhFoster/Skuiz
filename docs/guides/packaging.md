# Building and packaging

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

Deliberately not a default workspace member — `cargo build -p
skuiz-vst3` is an explicit choice because of the
[licensing obligation](../formats/vst3.md#licensing). The bundle is a
directory (`My.vst3/Contents/MacOS/My` on macOS);
`examples/shared-gain/bundle-vst3.sh` builds it. Test in a real host;
there is no in-tree VST3 validator.

## Standalone

No packaging: `cargo run -p my-plugin --bin my-plugin-standalone`, or
`cargo build --release` and ship the binary. See
[standalone](../formats/standalone.md).

## What CI checks

Every push to `main` and every PR runs rustfmt, clippy with warnings
denied, the full test suite on macOS/Windows/Linux, `cargo doc` with
warnings denied, clap-validator over all three examples, and the
SolidJS editor's headless DOM check. The same commands work locally —
see [contributing](../contributing.md).

## Not covered here

Code signing and notarization for distribution, installer building, and
AUv3/Xcode packaging (which lives in `crates/skuiz-auv3/scaffold/`).
