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
`skuiz-core = { git = "https://github.com/SarahhhhFoster/Skuiz" }` etc.

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

## Shipping: skuiz-package

`tools/skuiz-package` is the end-to-end packager: point it at a plugin
project (an example in this repo or a scaffolded standalone project) and
it runs the cargo build, assembles the plugin bundles and standalone app,
validates the CLAP bundle when `clap-validator` is on PATH, and emits any
selected permutation of `.dmg`, `.AppImage`, and `.exe`:

```sh
cargo run -p skuiz-package -- path/to/my-gain              # every format the host can build
cargo run -p skuiz-package -- my-gain --dmg --vst3         # pick formats
cargo run -p skuiz-package -- my-gain --exe --installer    # Windows: standalone + Inno installer
cargo run -p skuiz-package -- my-gain --dry-run            # show the resolved plan only
```

Identity (name, version, bundle id) is derived from the project's
`Cargo.toml` and `PluginInfo`; override with `--name` / `--version` /
`--identifier`. Useful flags: `--debug`, `--features a,b`, `--vst3`,
`--target <triple>`, `--no-standalone`, `--no-plugins`, `--icon`,
`--out <dir>`, `--appimagetool <path>`, `--iscc <path>`,
`--skip-validation`. `--help` prints the full list.

Formats are host-bound, because the packaging tools are:

- **.dmg** — macOS only (`hdiutil`). Contains the `.clap` bundle, the
  `.vst3` bundle (with `--vst3`), the standalone `.app`, and an
  INSTALL.txt.
- **.AppImage** — Linux only (`appimagetool` on PATH or via
  `--appimagetool`). Wraps the standalone app; a project without a
  `src/bin` binary can't produce one.
- **.exe** — Windows, or cross-compiled with `--target
  x86_64-pc-windows-msvc`. The standalone binary as
  `<name>-<version>-windows.exe`; `--installer` additionally generates an
  Inno Setup script and compiles it with `iscc` (installs plugins to
  `%COMMONPROGRAMFILES%\CLAP|VST3`, the app to Program Files).

## What CI checks

CI runs on pushes to `main` and on pull requests (feature branches are
manual-only, to keep Actions cost bounded). It checks rustfmt, clippy with
warnings denied, the full test suite on macOS/Windows/Linux, `cargo doc` with
warnings denied, clap-validator and Steinberg's VST3 `validator` over the
example bundles, and the SolidJS editor's headless DOM check. The same
commands work locally — see [contributing](../contributing.md).

## Not covered here

Code signing and notarization for distribution, and AUv3/Xcode packaging
(which lives in `crates/skuiz-auv3/scaffold/`).
