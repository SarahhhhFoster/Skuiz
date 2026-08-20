# Getting started

Build a working plugin — audio, an editor, and instance sync — in about
fifteen minutes. Everything here is complete: no steps are elided, and the
finished result matches `examples/shared-gain` in this repository.

**Prerequisites:** a Rust toolchain (`rustup`, stable). On macOS you also
need Xcode Command Line Tools (`xcode-select --install`). Nothing else — no
JavaScript toolchain, no plugin SDK to download.

> **Rather start from working code?** The scaffolder copies an example out
> of the repo, renames it, and rewires its manifest so it builds standalone:
>
> ```sh
> ./tools/new-plugin.sh my-gain                 # from examples/shared-gain
> ./tools/new-plugin.sh my-synth solid-synth    # or any other example
> cd my-gain && ./install.sh                    # build + install for the current user
> ```
>
> The generated project carries its own `bundle.sh` and `install.sh`. The
> steps below explain what that generated code actually does.

## 1. Create the crate

A plugin is a `cdylib`: a shared library the host loads.

```sh
cargo new --lib my-gain
cd my-gain
```

Replace `Cargo.toml` with:

```toml
[package]
name = "my-gain"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
skuiz-core = { git = "https://github.com/sarahhhh/skuiz" }
skuiz-clap = { git = "https://github.com/sarahhhh/skuiz" }
```

`crate-type = ["cdylib"]` is what makes the build produce a loadable library
rather than a Rust-only `rlib`. Skuiz is not yet published to crates.io, so
depend on it by path or git.

## 2. Implement the Processor

Everything a plugin does lives in one trait implementation. Put this in
`src/lib.rs`:

```rust
use skuiz_core::{MidiOut, ParamDef, PluginInfo, Processor};

/// Parameter ids are yours to choose, but must stay stable forever:
/// saved projects refer to them.
const P_GAIN: u32 = 0;

pub struct MyGain {
    gain: f64,
}

/// Skuiz creates instances with `Default`, so this is your initial state.
impl Default for MyGain {
    fn default() -> Self {
        Self { gain: 1.0 }
    }
}

impl Processor for MyGain {
    fn info() -> PluginInfo {
        PluginInfo {
            // Reverse-DNS and globally unique. Never change it after
            // release: hosts key saved projects on it.
            id: "com.example.my-gain",
            name: "My Gain",
            vendor: "Example",
            version: env!("CARGO_PKG_VERSION"),
            description: "A gain plugin",
        }
    }

    fn params() -> &'static [ParamDef] {
        &[ParamDef {
            id: P_GAIN,
            name: "Gain",
            min: 0.0,
            max: 1.0,
            default: 1.0,
            choices: &[], // empty = continuous, not a dropdown
        }]
    }

    fn set_param(&mut self, id: u32, value: f64) {
        if id == P_GAIN {
            // Clamp: hosts are not obliged to respect your declared range.
            self.gain = value.clamp(0.0, 1.0);
        }
    }

    fn get_param(&self, id: u32) -> f64 {
        if id == P_GAIN {
            self.gain
        } else {
            0.0
        }
    }

    fn process(&mut self, channels: &mut [&mut [f32]], _midi: &mut MidiOut) {
        let g = self.gain as f32;
        for ch in channels.iter_mut() {
            for sample in ch.iter_mut() {
                *sample *= g;
            }
        }
    }
}

// Exports the entry point the host looks for.
skuiz_clap::export_clap!(MyGain);
```

That is a complete, working plugin. Three things are worth noticing:

- **`process` works in place.** Each slice arrives holding input and must
  leave holding output.
- **`process` is realtime.** No allocation, no locking, no I/O, no
  panicking. See [threading](concepts/threading.md) — it is the one page
  that will save you a support ticket.
- **You never wrote format-specific code.** `export_clap!` is the only line
  that mentions CLAP, and adding VST3 later is one more line.

## 3. Build and install

```sh
cargo build --release
```

A CLAP plugin is a directory with a specific shape. On macOS:

```sh
BUNDLE=~/Library/Audio/Plug-Ins/CLAP/my-gain.clap
mkdir -p "$BUNDLE/Contents/MacOS"
cp target/release/libmy_gain.dylib "$BUNDLE/Contents/MacOS/my-gain"
cat > "$BUNDLE/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>my-gain</string>
    <key>CFBundleIdentifier</key><string>com.example.my-gain</string>
    <key>CFBundlePackageType</key><string>BNDL</string>
</dict>
</plist>
PLIST
```

On Linux, copy `libmy_gain.so` to `~/.clap/my-gain.clap` — no bundle needed.
On Windows, copy `my_gain.dll` to
`%COMMONPROGRAMFILES%\CLAP\my-gain.clap`.

See [packaging](guides/packaging.md) for scripts, and for the other formats.

## 4. Check it before loading it

Before opening a DAW, run the official CLAP validator. It catches contract
violations a host would only show you as mysterious misbehaviour:

```sh
cargo install --git https://github.com/free-audio/clap-validator.git
clap-validator validate ~/Library/Audio/Plug-Ins/CLAP/my-gain.clap
```

You want `0 failed`. Make this a habit — it is the single most useful check
in plugin development.

Now open your DAW, load **My Gain**, and the Gain parameter will appear in
the host's generic UI and respond to automation.

## 5. Add an editor

Skuiz editors are webviews, so an editor is an HTML document. Create
`src/editor.html`:

```html
<!doctype html>
<html>
<body style="font: 13px system-ui; background: #1e1e22; color: #ddd; margin: 16px">
  <label>Gain <span id="val">1.000</span></label>
  <input id="gain" type="range" min="0" max="1" step="0.001" value="1"
         style="width: 100%">
  <script>
    const slider = document.getElementById('gain');
    const val = document.getElementById('val');

    // UI -> plugin. Parameter id 0 is P_GAIN.
    slider.addEventListener('input', () => {
      val.textContent = Number(slider.value).toFixed(3);
      window.ipc.postMessage('set_param 0 ' + slider.value);
    });

    // plugin -> UI: host automation, preset loads, other instances.
    window.skuizOnParam = (id, value) => {
      if (id === 0) {
        slider.value = value;
        val.textContent = Number(value).toFixed(3);
      }
    };
  </script>
</body>
</html>
```

Then add two methods to your `impl Processor`:

```rust
    fn editor_html() -> Option<&'static str> {
        Some(include_str!("editor.html"))
    }

    fn editor_size() -> (u32, u32) {
        (320, 120)
    }
```

`include_str!` compiles the page into the binary, so there is no asset to
install alongside the plugin. Rebuild, reinstall, and the host will show
your editor instead of its generic one.

The two halves of the bridge — `window.ipc.postMessage` out and
`window.skuizOnParam` in — are the entire protocol. See
[editors](concepts/editors.md), including how to use a real framework.

## 6. Try instance sync

You did not write any code for this step. Load **two** instances of My Gain
in your DAW, open both editors, and drag one slider: the other follows.

Instances discover each other automatically, in the same process or across
processes. That is the feature Skuiz exists for, and
[instance sync](concepts/instance-sync.md) explains how it works and how to
send your own messages rather than just parameters.

## 7. Ship more formats

One line each, no other changes:

```rust
skuiz_vst3::export_vst3!(MyGain);   // also needs the skuiz-vst3 dependency
```

Shipping a VST3 binary is obligation-free: [the VST3 SDK has been
MIT-licensed](formats/vst3.md#licensing) since v3.8 (October 2025). The
only condition is trademark — branding with the "VST" name or logo means
following Steinberg's usage guidelines. For AUv3 see [its
guide](formats/auv3.md); for a desktop app see
[standalone](formats/standalone.md).

## Where to go next

- [Threading and realtime rules](concepts/threading.md) — the correctness
  page; read it before shipping.
- [Parameters](concepts/parameters.md) — dropdowns, saved state, ranges.
- [Writing DSP](guides/dsp.md) — C over FFI, or an embedded Pure Data patch.
- `examples/` in this repository — three complete plugins, all CI-tested.
