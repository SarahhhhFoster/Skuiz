//! skuiz-standalone: run a [`skuiz_core::Processor`] as a desktop app.
//!
//! One call, [`run`], gives a processor a window, its webview editor, an
//! audio device, and a seat on the instance bus — so a standalone instance
//! and a plugin instance loaded in a DAW sync with each other. That is the
//! cross-process tier of `skuiz-ipc` doing real work rather than a
//! hypothetical.
//!
//! # On Tauri
//!
//! PLAN.md named Tauri for this shell. What is used here is **tao + wry**:
//! Tauri's own window and webview layers, without the surrounding app
//! framework. The reason is code sharing, not dislike of Tauri. `skuiz-ui`
//! already drives wry to embed the editor in a plugin window, so building
//! the standalone on the same layer means one webview code path and one
//! editor contract for both. Adopting the full framework would add a second
//! UI stack, a JS build step and a config file, in exchange for packaging
//! features (bundling, updater, tray) this does not yet need. Those are a
//! packaging concern and can be added later by wrapping this shell, without
//! touching audio or editor code.
//!
//! # Audio
//!
//! Output only, with a built-in test tone as the processor's input. Capturing
//! the system input would mean running a second device and reconciling two
//! clocks with a drift-tolerant ring buffer, which is real work that nothing
//! here needs yet: a tone makes an effect audible and its parameters
//! demonstrable. See [`Input`] to choose silence instead.

#![warn(missing_docs)]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use skuiz_core::{MidiOut, Processor};
use std::sync::{Arc, Mutex};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;

/// Frames the deinterleave scratch can hold. Larger host buffers are
/// processed in several passes, so this never has to grow on the audio
/// thread.
const SCRATCH_FRAMES: usize = 2048;

/// Events the UI thread must handle, raised from other threads.
enum UserEvent {
    /// A parameter changed on another instance, via the bus.
    RemoteParam(u32, f64),
}

/// What the processor receives as input.
#[derive(Clone, Copy, PartialEq)]
pub enum Input {
    /// A 440 Hz sine at -12 dBFS, so effects are audible with no routing.
    TestTone,
    /// Silence, for processors that generate their own sound.
    Silence,
}

/// Why [`run`] could not start.
#[derive(Debug)]
pub enum Error {
    /// The system reported no usable audio output device.
    NoOutputDevice,
    /// The output device would not describe a usable configuration.
    Config(String),
    /// The audio stream could not be created or started.
    Audio(String),
    /// The application window could not be created.
    Window(String),
    /// The webview could not be attached to the window.
    WebView(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoOutputDevice => write!(f, "no audio output device is available"),
            Error::Config(e) => write!(f, "audio configuration failed: {e}"),
            Error::Audio(e) => write!(f, "audio stream failed: {e}"),
            Error::Window(e) => write!(f, "window creation failed: {e}"),
            Error::WebView(e) => write!(f, "webview creation failed: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// The processor's input source: the test tone oscillator, or silence.
/// One value per instance, stepped a frame at a time on the audio thread.
struct Tone {
    input: Input,
    phase: f32,
    step: f32,
}

impl Tone {
    fn new(input: Input, sample_rate: f32) -> Self {
        Self {
            input,
            phase: 0.0,
            step: std::f32::consts::TAU * 440.0 / sample_rate,
        }
    }

    fn next(&mut self) -> f32 {
        match self.input {
            Input::TestTone => {
                self.phase = (self.phase + self.step) % std::f32::consts::TAU;
                self.phase.sin() * 0.25
            }
            Input::Silence => 0.0,
        }
    }
}

/// Deinterleaved scratch buffers, allocated once and reused every callback.
struct Scratch {
    channels: Vec<Vec<f32>>,
}

impl Scratch {
    fn new(channel_count: usize) -> Self {
        Self {
            channels: (0..channel_count)
                .map(|_| vec![0.0; SCRATCH_FRAMES])
                .collect(),
        }
    }
}

/// Fill `scratch` with this pass's input and run the processor over it.
///
/// Split out from the stream callback so it can be tested without an audio
/// device: it is where interleaving, the tone and the processor meet.
///
/// `channel_count` is the *device* channel count, used to deinterleave
/// `out`; `scratch` holds the *processor* channel count, clamped to stereo.
/// The two differ on devices with more than two outputs — the stream
/// callback duplicates the last produced channel over the extras.
fn render_pass<P: Processor>(
    processor: &Mutex<P>,
    midi: &mut MidiOut,
    scratch: &mut Scratch,
    out: &mut [f32],
    channel_count: usize,
    tone: &mut Tone,
) {
    let frames = out.len() / channel_count.max(1);
    for frame in 0..frames {
        let sample = tone.next();
        for ch in scratch.channels.iter_mut() {
            ch[frame] = sample;
        }
    }

    {
        // Borrow each channel's live region as a separate mutable slice, on
        // the stack: a Vec here would allocate on the audio thread, which is
        // the one thing this function promises not to do.
        let mut views: [&mut [f32]; 2] = [&mut [], &mut []];
        let used = scratch.channels.len().min(views.len());
        for (view, chan) in views.iter_mut().zip(scratch.channels.iter_mut()) {
            *view = &mut chan[..frames];
        }
        // A poisoned lock is recovered, not skipped: a panic on another
        // thread must not silence the audio thread or drop edits. Same
        // policy as `skuiz_core::snapshot_params`.
        processor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .process(&mut views[..used], midi);
    }

    for frame in 0..frames {
        for (ch, buf) in scratch.channels.iter().enumerate() {
            out[frame * channel_count + ch] = buf[frame];
        }
    }
}

/// Run `P` as a standalone application. Blocks until the window closes,
/// then calls [`Processor::deactivate`] before returning.
pub fn run<P: Processor + Default>(input: Input) -> Result<(), Error> {
    let info = P::info();

    // --- shared state -----------------------------------------------------

    let processor = Arc::new(Mutex::new(P::default()));
    for def in P::params() {
        processor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_param(def.id, def.default);
    }

    // --- window and event loop -------------------------------------------

    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let (width, height) = P::editor_size();
    let window = WindowBuilder::new()
        .with_title(info.name)
        .with_inner_size(tao::dpi::LogicalSize::new(width, height))
        .build(&event_loop)
        .map_err(|e| Error::Window(e.to_string()))?;

    // --- instance bus -----------------------------------------------------
    //
    // The callback runs on a bus thread (or another instance's thread), so it
    // only hands the value to the UI thread rather than touching state here.
    let proxy = event_loop.create_proxy();
    let bus = Arc::new(skuiz_ipc::Bus::join(info.id, move |frame| {
        let Ok(msg) = std::str::from_utf8(frame) else {
            return;
        };
        let Some((id, value)) = skuiz_core::protocol::parse_set_param(msg) else {
            return;
        };
        let _ = proxy.send_event(UserEvent::RemoteParam(id, value));
    }));

    // --- audio ------------------------------------------------------------

    let host = cpal::default_host();
    let device = host.default_output_device().ok_or(Error::NoOutputDevice)?;
    let supported = device
        .default_output_config()
        .map_err(|e| Error::Config(e.to_string()))?;
    let sample_rate = supported.sample_rate() as f64;
    let device_channels = supported.channels() as usize;
    // The Processor contract covers up to stereo; extra device channels are
    // fed from the last one we produce.
    let channel_count = device_channels.clamp(1, 2);

    processor
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .activate(sample_rate, SCRATCH_FRAMES as u32);

    let stream = {
        let processor = Arc::clone(&processor);
        let mut scratch = Scratch::new(channel_count);
        let mut midi = MidiOut::with_capacity(512);
        let mut tone = Tone::new(input, sample_rate as f32);
        let config: cpal::StreamConfig = supported.into();

        device
            .build_output_stream(
                config,
                move |data: &mut [f32], _| {
                    // ponytail: generated MIDI is drained and dropped; the
                    // standalone has no MIDI destination until a virtual
                    // port (midir) is wired up.
                    midi.clear();

                    // Process in scratch-sized passes so no buffer size can
                    // force an allocation on the audio thread.
                    let max_chunk = SCRATCH_FRAMES * device_channels;
                    for chunk in data.chunks_mut(max_chunk) {
                        render_pass(
                            &processor,
                            &mut midi,
                            &mut scratch,
                            chunk,
                            device_channels,
                            &mut tone,
                        );
                        // Duplicate the last produced channel across any
                        // extra device channels (e.g. mono source, 4-out
                        // interface) so nothing is left uninitialised.
                        if device_channels > channel_count {
                            let frames = chunk.len() / device_channels;
                            for frame in 0..frames {
                                let base = frame * device_channels;
                                let last = chunk[base + channel_count - 1];
                                for ch in channel_count..device_channels {
                                    chunk[base + ch] = last;
                                }
                            }
                        }
                    }
                },
                |err| eprintln!("skuiz: audio stream error: {err}"),
                None,
            )
            .map_err(|e| Error::Audio(e.to_string()))?
    };
    stream.play().map_err(|e| Error::Audio(e.to_string()))?;

    // --- editor -----------------------------------------------------------

    let html = P::editor_html().unwrap_or(
        "<!doctype html><body style='font:13px system-ui;padding:1rem'>\
         This processor has no editor.</body>",
    );
    let ui_processor = Arc::clone(&processor);
    let ui_bus = Arc::clone(&bus);
    let webview = wry::WebViewBuilder::new()
        .with_html(html)
        .with_ipc_handler(move |req| {
            let Some((id, value)) = skuiz_core::protocol::parse_set_param(req.body()) else {
                return;
            };
            if !P::params().iter().any(|d| d.id == id) {
                return;
            }
            ui_processor
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_param(id, value);
            // Share the move with every other instance, in this process or
            // in a DAW hosting the same plugin.
            ui_bus.send(skuiz_core::protocol::set_param(id, value).as_bytes());
        })
        .build(&window)
        .map_err(|e| Error::WebView(e.to_string()))?;

    // Seed the page with the current values — snapshot first so the
    // processor lock is not held across the eval calls, which the audio
    // callback would otherwise block on.
    //
    // These evals race page load: one landing before the page has installed
    // `window.skuizOnParam` is a no-op (the generated call is guarded). The
    // editor contract therefore requires pages to push their own state on
    // mount — that, not this seeding, is what the examples rely on.
    for (id, value) in skuiz_core::snapshot_params::<P>(&processor) {
        let _ = webview.evaluate_script(&skuiz_core::protocol::on_param_js(id, value));
    }

    // --- run --------------------------------------------------------------

    // `run_return` rather than tao's `run`: `run` never returns, which would
    // make `deactivate` unreachable.
    let rt_processor = Arc::clone(&processor);
    event_loop.run_return(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UserEvent::RemoteParam(id, value)) => {
                if !P::params().iter().any(|d| d.id == id) {
                    return;
                }
                rt_processor
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .set_param(id, value);
                // Update the page, but do not echo back onto the bus, or two
                // instances would ping-pong a value forever.
                let _ = webview.evaluate_script(&skuiz_core::protocol::on_param_js(id, value));
            }
            _ => {}
        }
    });

    // The window has closed; release what `activate` set up, on the main
    // thread as the Processor contract requires.
    processor
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .deactivate();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use skuiz_core::{ParamDef, PluginInfo};

    struct Gain(f64);

    impl Default for Gain {
        fn default() -> Self {
            Self(1.0)
        }
    }

    impl Processor for Gain {
        fn info() -> PluginInfo {
            PluginInfo {
                id: "test.sa",
                name: "g",
                vendor: "t",
                version: "0",
                description: "",
            }
        }
        fn params() -> &'static [ParamDef] {
            &[ParamDef {
                id: 0,
                name: "Gain",
                min: 0.0,
                max: 1.0,
                default: 1.0,
                choices: &[],
            }]
        }
        fn set_param(&mut self, _id: u32, v: f64) {
            self.0 = v;
        }
        fn get_param(&self, _id: u32) -> f64 {
            self.0
        }
        fn process(&mut self, channels: &mut [&mut [f32]], _midi: &mut MidiOut) {
            let g = self.0 as f32;
            for ch in channels.iter_mut() {
                for s in ch.iter_mut() {
                    *s *= g;
                }
            }
        }
    }

    #[test]
    fn parses_ui_messages() {
        assert_eq!(
            skuiz_core::protocol::parse_set_param("set_param 3 0.25"),
            Some((3, 0.25))
        );
        assert_eq!(skuiz_core::protocol::parse_set_param("set_param 3"), None);
        assert_eq!(
            skuiz_core::protocol::parse_set_param("something_else 3 0.25"),
            None
        );
        assert_eq!(skuiz_core::protocol::parse_set_param(""), None);
    }

    /// The audio path without an audio device: interleaving, the tone, and
    /// the processor's effect on it.
    #[test]
    fn render_pass_interleaves_and_applies_the_processor() {
        let processor = Mutex::new(Gain(1.0));
        let mut midi = MidiOut::with_capacity(8);
        let mut scratch = Scratch::new(2);
        let mut out = vec![0.0f32; 64 * 2];
        let mut tone = Tone::new(Input::TestTone, 48_000.0);

        render_pass(&processor, &mut midi, &mut scratch, &mut out, 2, &mut tone);
        assert!(
            out.iter().any(|s| s.abs() > 0.0),
            "test tone produced silence"
        );
        assert!(
            out.iter().all(|s| s.abs() <= 0.25 + 1e-6),
            "tone exceeded -12 dBFS"
        );
        // Interleaved stereo: both channels carry the same mono tone.
        for frame in 0..64 {
            assert_eq!(out[frame * 2], out[frame * 2 + 1], "channels must match");
        }

        // Gain of zero must silence it, proving the processor is in the path.
        let loud = out.clone();
        processor.lock().unwrap().0 = 0.0;
        tone.phase = 0.0;
        render_pass(&processor, &mut midi, &mut scratch, &mut out, 2, &mut tone);
        assert!(loud.iter().any(|s| s.abs() > 0.0));
        assert!(
            out.iter().all(|s| *s == 0.0),
            "gain 0 did not silence the output"
        );
    }

    #[test]
    fn silence_input_produces_silence() {
        let processor = Mutex::new(Gain(1.0));
        let mut midi = MidiOut::with_capacity(8);
        let mut scratch = Scratch::new(1);
        let mut out = vec![1.0f32; 32];
        let mut tone = Tone::new(Input::Silence, 48_000.0);

        render_pass(&processor, &mut midi, &mut scratch, &mut out, 1, &mut tone);
        assert!(
            out.iter().all(|s| *s == 0.0),
            "Silence input must clear the buffer"
        );
    }

    /// A buffer larger than the scratch must still be filled completely,
    /// since that is the case where a naive implementation would allocate.
    #[test]
    fn oversized_buffers_are_processed_in_passes() {
        let processor = Mutex::new(Gain(1.0));
        let mut midi = MidiOut::with_capacity(8);
        let mut scratch = Scratch::new(1);
        let mut tone = Tone::new(Input::TestTone, 48_000.0);

        let total = SCRATCH_FRAMES * 2 + 37;
        let mut out = vec![f32::NAN; total];
        for chunk in out.chunks_mut(SCRATCH_FRAMES) {
            render_pass(&processor, &mut midi, &mut scratch, chunk, 1, &mut tone);
        }
        assert!(
            out.iter().all(|s| s.is_finite()),
            "some frames were never written"
        );
    }
}
