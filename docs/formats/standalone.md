# Standalone

Any processor runs as a desktop app — window, the same webview editor,
an audio device, and a seat on the instance bus — with one call:

```rust
fn main() {
    skuiz_standalone::run::<MyProcessor>(skuiz_standalone::Input::TestTone)
        .expect("standalone failed");
}
```

`run::<P: Processor + Default>(input)` blocks until the window closes,
then calls your `deactivate` before returning. See
`examples/*/src/bin/` for working mains:

```sh
cargo run -p solid-synth --bin solid-synth-standalone
```

## Why it exists

Run the standalone *alongside* your CLAP or VST3 in a DAW. They are
separate processes, so moving a slider in one drives the other — the
cheapest possible demonstration of the whole project. It is also a
genuinely useful development loop: no host, no plugin scanning, just
`cargo run`.

## Input: TestTone or Silence

The shell is **output-only** today. `Input::TestTone` feeds your
processor a 440 Hz tone at −12 dBFS so an effect is audible with no
routing; `Input::Silence` is for generators (solid-synth uses it).
Only the declared main buses are wired: a sidechain input stays
inactive, and an instrument topology (no inputs) simply gets no input
fed. Capturing system input needs a second device and drift-tolerant
buffering — deferred, see the README.

Extra device channels beyond your main output bus get the last
produced channel duplicated onto them; block sizes are normalized into
fixed-size passes so no device buffer size can force an allocation on
the audio thread.

## Limitations

- **No audio input** (above).
- **Generated MIDI is dropped** — the standalone drains `MidiOut` and
  discards it; MIDI output from the shell is deferred.
- **Editor platform support** is the webview's: macOS tested; Windows
  and Linux written but unverified (Linux embeds on X11 via WebKitGTK).

## Under the hood

Built on tao + wry — Tauri's window and webview layers — rather than
the full Tauri framework, so the standalone and the plugins share one
webview layer and one editor contract. The bus, parameters, state, and
editor all behave exactly as in a host; see
[threading](../concepts/threading.md) for what runs on which thread.
