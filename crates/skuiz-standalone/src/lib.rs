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
//! The obvious framework for this shell is Tauri. What is used here is
//! **tao + wry**:
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
//!
//! The shell wires up only the declared *main* buses: the tone (or silence)
//! feeds the main input, aliased in place onto the main output, and the main
//! output maps onto the device channels. A declared optional sidechain is
//! always inactive — there is nothing to feed it — and an instrument
//! topology (no input buses) simply gets no input.

#![warn(missing_docs)]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use skuiz_core::bus::{BusDirection, TopologyScratch};
use skuiz_core::engine::{AudioToken, Engine};
use skuiz_core::{MidiOut, Processor};
use std::sync::Arc;
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
/// Channel count comes from the declared main bus layouts — the main input
/// and output alias this memory, so it is sized to the wider of the two.
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

/// Channel count of the declared main bus in `dir` — 0 when the topology
/// has no bus in that direction (an instrument's inputs).
fn main_bus_channels<P: Processor>(dir: BusDirection) -> usize {
    P::audio_buses()
        .iter()
        .find(|s| s.direction == dir)
        .map_or(0, |s| s.layout.channels() as usize)
}

/// Fill `scratch` with this pass's input and run the processor over it.
///
/// Split out from the stream callback so it can be tested without an audio
/// device: it is where interleaving, the tone and the processor meet. The
/// caller owns the processor for the duration — the stream callback gets it
/// from the engine's audio side (invariant 1), so this function never
/// locks anything.
///
/// `device_channels` is the *device* channel count, used to deinterleave
/// `out`; `scratch` holds the *processor* channel count, derived from the
/// declared main bus layouts. The two differ on devices wider than the main
/// output — the stream callback duplicates the last produced channel over
/// the extras.
fn render_pass<P: Processor>(
    processor: &mut P,
    midi: &mut MidiOut,
    bus_scratch: &mut TopologyScratch,
    scratch: &mut Scratch,
    out: &mut [f32],
    device_channels: usize,
    tone: &mut Tone,
) {
    let frames = out.len() / device_channels.max(1);
    for frame in 0..frames {
        let sample = tone.next();
        for ch in scratch.channels.iter_mut() {
            ch[frame] = sample;
        }
    }

    // Wire the declared main buses onto the scratch. `set_channel` takes raw
    // pointers so the main input can alias the main output's memory, giving
    // the processor the same in-place buffers as before topologies were
    // declarative; the views below are the only access while they live.
    bus_scratch.clear();
    let out_channels = main_bus_channels::<P>(BusDirection::Output);
    if out_channels > 0 {
        bus_scratch.set_active(BusDirection::Output, 0, true);
        for (c, chan) in scratch.channels.iter_mut().take(out_channels).enumerate() {
            // SAFETY: the scratch buffers are owned here and live past the
            // process call; the views built from them die before the next
            // pass reuses them.
            unsafe {
                bus_scratch.set_channel(BusDirection::Output, 0, c, chan.as_mut_ptr(), frames);
            }
        }
    }
    if main_bus_channels::<P>(BusDirection::Input) > 0 {
        bus_scratch.set_active(BusDirection::Input, 0, true);
        for (c, chan) in scratch
            .channels
            .iter_mut()
            .take(main_bus_channels::<P>(BusDirection::Input))
            .enumerate()
        {
            // SAFETY: same buffers as the main output above — the alias is
            // the in-place contract, and no other access overlaps the pass.
            unsafe {
                bus_scratch.set_channel(BusDirection::Input, 0, c, chan.as_mut_ptr(), frames);
            }
        }
    }
    // Any further declared input (a sidechain) stays inactive: a standalone
    // has nothing to feed it.

    {
        let (inputs, mut outputs) = bus_scratch.views();
        processor.process(&inputs, &mut outputs, midi);
    }

    let mapped = out_channels.min(device_channels);
    for frame in 0..frames {
        for (ch, buf) in scratch.channels.iter().enumerate().take(mapped) {
            out[frame * device_channels + ch] = buf[frame];
        }
    }
}

/// Run `P` as a standalone application. Blocks until the window closes,
/// then calls [`Processor::deactivate`] before returning.
pub fn run<P: Processor + Default>(input: Input) -> Result<(), Error> {
    let info = P::info();

    // --- shared state -----------------------------------------------------

    // The engine owns the processor. The audio callback claims the audio
    // state around each block; the UI thread and the bus reach the
    // processor only through the engine's realtime-safe paths — no mutex
    // ever crosses onto the audio thread (invariants 1-2).
    let engine = Engine::<P>::new(512);
    // Stopped, so these apply directly and publish the readback to the
    // mirror (a processor may transform its defaults).
    for def in P::params() {
        engine.set_param(def.id, def.default);
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
    // Last-writer-wins versions for shared parameters (invariant 9); bus
    // and UI threads only.
    let lww = Arc::new(skuiz_core::lww::Lww::new());
    let lww_cb = Arc::clone(&lww);
    let bus_engine = Arc::clone(&engine);
    let sender_slot: Arc<std::sync::Mutex<Option<skuiz_ipc::BusSender>>> =
        Arc::new(std::sync::Mutex::new(None));
    let cb_sender = Arc::clone(&sender_slot);
    let bus = Arc::new(skuiz_ipc::Bus::join(info.id, move |frame| {
        use skuiz_core::protocol as proto;
        if frame == skuiz_ipc::LINK_UP_FRAME {
            // Link (back) up: re-sync so frames dropped while the
            // cross-process link was down heal (invariant 9).
            if let Some(sender) = cb_sender.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                sender.send(proto::sync_request(lww_cb.origin()).as_bytes());
            }
            return;
        }
        let Ok(msg) = std::str::from_utf8(frame) else {
            return;
        };
        if let Some((id, value, version)) = proto::parse_set_param_versioned(msg) {
            // Local parameters never sync: frames naming them are dropped
            // here rather than posted (invariant 10). Stale versions lose.
            // The version is recorded only if the change was queued to the
            // UI thread, so a lost frame can still win when re-delivered.
            if !skuiz_core::syncs_over_bus::<P>(id) {
                return;
            }
            lww_cb.accept_with(id, version, || {
                proxy.send_event(UserEvent::RemoteParam(id, value)).is_ok()
            });
            return;
        }
        if proto::parse_sync_request(msg).is_some() {
            // A late joiner asked for shared state; answer from the mirror
            // (wait-free, invariant 6) with the parameters we hold a *fresh*
            // version for — ones edited over the bus and not rewritten by a
            // project load since. Never-edited and post-load parameters are
            // omitted: their value is host automation or project state,
            // which is per-instance (invariant 10). LWW makes duplicate
            // answers safe.
            let entries: Vec<(u32, f64, u64, u64)> = bus_engine
                .mirror()
                .snapshot()
                .into_iter()
                .filter(|(id, _)| skuiz_core::syncs_over_bus::<P>(*id))
                .filter_map(|(id, value)| {
                    lww_cb
                        .advertised_version(id)
                        .map(|(seq, origin)| (id, value, seq, origin))
                })
                .collect();
            if !entries.is_empty() {
                if let Some(sender) = cb_sender.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    sender.send(proto::sync_state(&entries).as_bytes());
                }
            }
            return;
        }
        if let Some(entries) = proto::parse_sync_state(msg) {
            for (id, value, seq, origin) in entries {
                if skuiz_core::syncs_over_bus::<P>(id) {
                    lww_cb.accept_with(id, Some((seq, origin)), || {
                        proxy.send_event(UserEvent::RemoteParam(id, value)).is_ok()
                    });
                }
            }
        }
    }));
    *sender_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(bus.sender());
    // Late joiner: ask the bus for current shared state.
    bus.send(skuiz_core::protocol::sync_request(lww.origin()).as_bytes());

    // --- audio ------------------------------------------------------------

    let host = cpal::default_host();
    let device = host.default_output_device().ok_or(Error::NoOutputDevice)?;
    let supported = device
        .default_output_config()
        .map_err(|e| Error::Config(e.to_string()))?;
    let sample_rate = supported.sample_rate() as f64;
    let device_channels = supported.channels() as usize;
    // The declared main output layout decides how many processor channels
    // the device gets; extra device channels are fed from the last one we
    // produce.
    let processor_channels = main_bus_channels::<P>(BusDirection::Output).max(1);
    let channel_count = device_channels.clamp(1, processor_channels);

    let _ = engine.with_main(|core| {
        core.processor.activate(sample_rate, SCRATCH_FRAMES as u32);
    });

    let stream = {
        let engine = Arc::clone(&engine);
        // The main input aliases the main output, so the scratch must hold
        // the wider of the two declared layouts.
        let scratch_channels = main_bus_channels::<P>(BusDirection::Input)
            .max(main_bus_channels::<P>(BusDirection::Output));
        let mut scratch = Scratch::new(scratch_channels);
        let mut tone = Tone::new(input, sample_rate as f32);
        // Proof the engine is in the AUDIO state, held for the duration of
        // each callback: claimed at the top, handed back at the bottom.
        let mut audio_token: Option<AudioToken> = None;
        let config: cpal::StreamConfig = supported.into();

        device
            .build_output_stream(
                config,
                move |data: &mut [f32], _| {
                    // Claim the audio side for the duration of the callback
                    // (tolerates a backend that starts calling before play()
                    // returns), drain anything the UI or the bus queued, then
                    // process. MIDI scratch lives in the core.
                    if !engine.is_processing() {
                        audio_token = Some(engine.begin_audio());
                    }
                    let core =
                        engine.audio_core(audio_token.as_ref().expect("AUDIO implies a token"));
                    let report = engine.drain_commands(core);
                    let _ = report; // no host to notify in a standalone shell
                    core.midi_out.clear();

                    // Process in scratch-sized passes so no buffer size can
                    // force an allocation on the audio thread.
                    let max_chunk = SCRATCH_FRAMES * device_channels;
                    for chunk in data.chunks_mut(max_chunk) {
                        render_pass(
                            &mut core.processor,
                            &mut core.midi_out,
                            &mut core.bus_scratch,
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
                    // ponytail: generated MIDI is drained and dropped; the
                    // standalone has no MIDI destination until a virtual
                    // port (midir) is wired up.
                    if let Some(token) = audio_token.take() {
                        engine.end_audio(token);
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
    let ui_engine = Arc::clone(&engine);
    let ui_bus = Arc::clone(&bus);
    let ui_lww = Arc::clone(&lww);
    // The IPC handler answers diag queries with an eval, but the webview
    // only exists after build — hence the cell, filled below.
    let webview_slot: std::rc::Rc<std::cell::RefCell<Option<wry::WebView>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let handler_slot = std::rc::Rc::clone(&webview_slot);
    let webview = wry::WebViewBuilder::new()
        .with_html(html)
        .with_ipc_handler(move |req| {
            if req.body() == skuiz_core::protocol::DIAG_QUERY {
                let js = skuiz_core::protocol::on_diag_js(ui_engine.diag());
                if let Some(wv) = handler_slot.borrow().as_ref() {
                    let _ = wv.evaluate_script(&js);
                }
                return;
            }
            let Some((id, value)) = skuiz_core::protocol::parse_set_param(req.body()) else {
                return;
            };
            let Some(def) = P::params().iter().find(|d| d.id == id) else {
                return;
            };
            // Queued for the audio callback; the engine never locks the
            // processor. For shared parameters the apply happens inside
            // `stamp_with`: only a change that entered the engine claims a
            // version and reaches the bus (invariant 9).
            if def.shared {
                // Share the move with every other instance, in this process
                // or in a DAW hosting the same plugin. The versioned frame
                // lets receivers discard stale echoes.
                if let Some((seq, origin)) =
                    ui_lww.stamp_with(id, || ui_engine.set_param(id, value))
                {
                    ui_bus.send(
                        skuiz_core::protocol::set_param_versioned(id, value, seq, origin)
                            .as_bytes(),
                    );
                }
            } else {
                ui_engine.set_param(id, value);
            }
        })
        .build(&window)
        .map_err(|e| Error::WebView(e.to_string()))?;

    // Seed the page with the current values — read from the mirror
    // (wait-free, invariant 6), never from the processor.
    //
    // These evals race page load: one landing before the page has installed
    // `window.skuizOnParam` is a no-op (the generated call is guarded). The
    // editor contract therefore requires pages to push their own state on
    // mount — that, not this seeding, is what the examples rely on.
    for (id, value) in engine.mirror().snapshot() {
        let _ = webview.evaluate_script(&skuiz_core::protocol::on_param_js(id, value));
    }
    // Hand the webview to the IPC handler's cell, so diag queries can be
    // answered from inside a message.
    *webview_slot.borrow_mut() = Some(webview);

    // --- run --------------------------------------------------------------

    // `run_return` rather than tao's `run`: `run` never returns, which would
    // make `deactivate` unreachable.
    let rt_engine = Arc::clone(&engine);
    let rt_webview = std::rc::Rc::clone(&webview_slot);
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
                // Queued for the audio callback, same as an editor move.
                rt_engine.set_param(id, value);
                // Update the page, but do not echo back onto the bus, or two
                // instances would ping-pong a value forever.
                if let Some(wv) = rt_webview.borrow().as_ref() {
                    let _ = wv.evaluate_script(&skuiz_core::protocol::on_param_js(id, value));
                }
            }
            _ => {}
        }
    });

    // The window has closed. Stop the stream first so no callback can race
    // `deactivate`, then release what `activate` set up, on the main thread
    // as the Processor contract requires.
    drop(stream);
    while engine.is_processing() {
        std::hint::spin_loop();
    }
    let _ = engine.with_main(|core| {
        core.processor.deactivate();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use skuiz_core::bus::{AudioBusSpec, ChannelLayout};
    use skuiz_core::{AudioInputs, AudioOutputs, ParamDef, PluginInfo};

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
                shared: true,
            }]
        }
        fn set_param(&mut self, _id: u32, v: f64) {
            self.0 = v;
        }
        fn get_param(&self, _id: u32) -> f64 {
            self.0
        }
        fn process(
            &mut self,
            _inputs: &AudioInputs,
            outputs: &mut AudioOutputs,
            _midi: &mut MidiOut,
        ) {
            let g = self.0 as f32;
            let Some(main) = outputs.main() else {
                return;
            };
            for ch in main.channels() {
                for s in ch.iter_mut() {
                    *s *= g;
                }
            }
        }
    }

    /// An effect with an optional sidechain input: records what the shell
    /// actually fed it, so the test can see the standalone's wiring.
    #[derive(Default)]
    struct SidechainFx {
        sidechain_active: Option<bool>,
        main_channels: usize,
    }

    impl Processor for SidechainFx {
        fn info() -> PluginInfo {
            PluginInfo {
                id: "test.sc",
                name: "sc",
                vendor: "t",
                version: "0",
                description: "",
            }
        }
        fn params() -> &'static [ParamDef] {
            &[]
        }
        fn set_param(&mut self, _id: u32, _v: f64) {}
        fn get_param(&self, _id: u32) -> f64 {
            0.0
        }
        fn audio_buses() -> &'static [AudioBusSpec] {
            const BUSES: &[AudioBusSpec] = &[
                AudioBusSpec::input("Main", ChannelLayout::Stereo),
                AudioBusSpec::input("Sidechain", ChannelLayout::Mono).optional(),
                AudioBusSpec::output("Main", ChannelLayout::Stereo),
            ];
            BUSES
        }
        fn process(
            &mut self,
            inputs: &AudioInputs,
            outputs: &mut AudioOutputs,
            _midi: &mut MidiOut,
        ) {
            self.sidechain_active = inputs.at(1).map(|bus| bus.active());
            let Some(main) = outputs.main() else {
                return;
            };
            self.main_channels = main.channels().len();
            for ch in main.channels() {
                for s in ch.iter_mut() {
                    *s *= 2.0;
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
        let mut processor = Gain(1.0);
        let mut midi = MidiOut::with_capacity(8);
        let mut bus_scratch = TopologyScratch::new(Gain::audio_buses());
        let mut scratch = Scratch::new(2);
        let mut out = vec![0.0f32; 64 * 2];
        let mut tone = Tone::new(Input::TestTone, 48_000.0);

        render_pass(
            &mut processor,
            &mut midi,
            &mut bus_scratch,
            &mut scratch,
            &mut out,
            2,
            &mut tone,
        );
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
        processor.0 = 0.0;
        tone.phase = 0.0;
        render_pass(
            &mut processor,
            &mut midi,
            &mut bus_scratch,
            &mut scratch,
            &mut out,
            2,
            &mut tone,
        );
        assert!(loud.iter().any(|s| s.abs() > 0.0));
        assert!(
            out.iter().all(|s| *s == 0.0),
            "gain 0 did not silence the output"
        );
    }

    #[test]
    fn silence_input_produces_silence() {
        let mut processor = Gain(1.0);
        let mut midi = MidiOut::with_capacity(8);
        let mut bus_scratch = TopologyScratch::new(Gain::audio_buses());
        // The scratch follows the declared stereo topology even when the
        // device is mono; only the first channel maps to the device.
        let mut scratch = Scratch::new(2);
        let mut out = vec![1.0f32; 32];
        let mut tone = Tone::new(Input::Silence, 48_000.0);

        render_pass(
            &mut processor,
            &mut midi,
            &mut bus_scratch,
            &mut scratch,
            &mut out,
            1,
            &mut tone,
        );
        assert!(
            out.iter().all(|s| *s == 0.0),
            "Silence input must clear the buffer"
        );
    }

    /// A buffer larger than the scratch must still be filled completely,
    /// since that is the case where a naive implementation would allocate.
    #[test]
    fn oversized_buffers_are_processed_in_passes() {
        let mut processor = Gain(1.0);
        let mut midi = MidiOut::with_capacity(8);
        let mut bus_scratch = TopologyScratch::new(Gain::audio_buses());
        let mut scratch = Scratch::new(2);
        let mut tone = Tone::new(Input::TestTone, 48_000.0);

        let total = SCRATCH_FRAMES * 2 + 37;
        let mut out = vec![f32::NAN; total];
        for chunk in out.chunks_mut(SCRATCH_FRAMES) {
            render_pass(
                &mut processor,
                &mut midi,
                &mut bus_scratch,
                &mut scratch,
                chunk,
                1,
                &mut tone,
            );
        }
        assert!(
            out.iter().all(|s| s.is_finite()),
            "some frames were never written"
        );
    }

    /// A declared sidechain has no source in a standalone: the processor
    /// must see it inactive while the main pair is processed as usual.
    #[test]
    fn declared_sidechain_is_always_inactive() {
        let mut processor = SidechainFx::default();
        let mut midi = MidiOut::with_capacity(8);
        let mut bus_scratch = TopologyScratch::new(SidechainFx::audio_buses());
        let mut scratch = Scratch::new(2);
        let mut out = vec![0.0f32; 32 * 2];
        let mut tone = Tone::new(Input::TestTone, 48_000.0);

        render_pass(
            &mut processor,
            &mut midi,
            &mut bus_scratch,
            &mut scratch,
            &mut out,
            2,
            &mut tone,
        );
        assert_eq!(
            processor.sidechain_active,
            Some(false),
            "the standalone never feeds a sidechain"
        );
        assert_eq!(
            processor.main_channels, 2,
            "the main pair still ran, stereo"
        );
        assert!(
            out.iter().any(|s| s.abs() > 0.25 + 1e-6),
            "the main pair was processed (gain 2 on the -12 dBFS tone)"
        );
    }
}
