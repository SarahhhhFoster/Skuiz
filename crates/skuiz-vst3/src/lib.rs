//! skuiz-vst3: VST3 format adapter.
//!
//! Implement [`skuiz_core::Processor`] (plus `Default`) and export it from a
//! `cdylib` with `skuiz_vst3::export_vst3!(MyProcessor);`.
//!
//! # Licensing
//!
//! Skuiz is MIT and stays that way: this adapter builds on the clean-room
//! MIT/Apache-2.0 `vst3` bindings, and no Steinberg SDK code is vendored or
//! linked. Since Steinberg relicensed the VST3 SDK under MIT in v3.8
//! (October 2025), retiring the GPLv3-or-proprietary dual licence, shipping
//! a VST3 binary carries no Steinberg licensing obligation at all. The one
//! remaining condition is trademark: using the "VST" name or logo is
//! optional, but if you do, Steinberg's usage guidelines (bundled with the
//! SDK) apply.
//!
//! This crate was excluded from the workspace's default members before the
//! MIT relicensing; it is an ordinary member now.
//!
//! # Shape
//!
//! This is a *single-component* plugin: one object implements `IComponent`,
//! `IAudioProcessor` and `IEditController` together. VST3 also allows the
//! processor and controller to be separate objects, which then have to keep
//! duplicate parameter state in sync through the host; with a Skuiz
//! `Processor` already owning parameters and audio in one place, splitting
//! them would create a synchronisation problem rather than solve one.

#![allow(non_snake_case)]

use skuiz_core::bus::{
    AudioBusSpec, BusDirection as CoreBusDirection, ChannelLayout, MAX_BUSES_PER_DIRECTION,
    MAX_BUS_CHANNELS,
};
use skuiz_core::diag::DiagCounters;
use skuiz_core::engine::{AudioCore, AudioToken, Engine};
use skuiz_core::{MidiOut, ParamDef, Processor};
use std::cell::UnsafeCell;
use std::ffi::{c_char, c_void, CStr, CString};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComRef, ComWrapper};

pub use vst3;

/// Whether this platform has a webview editor backend in `skuiz-ui`.
const EDITOR_SUPPORTED: bool = cfg!(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux"
));

/// The VST3 platform type matching this platform's `ParentView` constructor.
#[doc(hidden)] // public for the roundtrip tests, not part of the API surface
pub fn native_platform_type() -> FIDString {
    #[cfg(target_os = "windows")]
    {
        kPlatformTypeHWND
    }
    #[cfg(target_os = "linux")]
    {
        kPlatformTypeX11EmbedWindowID
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "linux")))]
    {
        kPlatformTypeNSView
    }
}

/// Events one block may emit before further MIDI is dropped.
const MIDI_OUT_CAPACITY: usize = 512;

/// Timed parameter points one block may carry before the excess is dropped.
/// Like [`MidiOut`], the buffer is pre-allocated and fixed: a host sending
/// more automation points than this in a single block is pathological.
const PARAM_EVENT_CAPACITY: usize = 256;

// --- small helpers ------------------------------------------------------

/// Run `f`, returning `fallback` if it panics. A panic unwinding across the
/// COM boundary is UB and would abort the host, so entry points that reach
/// user code go through here.
#[doc(hidden)]
pub fn ffi_guard<R>(f: impl FnOnce() -> R, fallback: R) -> R {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(fallback)
}

fn copy_cstring(src: &str, dst: &mut [c_char]) {
    let c = CString::new(src).unwrap_or_default();
    let bytes = c.as_bytes_with_nul();
    for (s, d) in bytes.iter().zip(dst.iter_mut()) {
        *d = *s as c_char;
    }
    if bytes.len() > dst.len() {
        if let Some(last) = dst.last_mut() {
            *last = 0;
        }
    }
}

fn copy_wstring(src: &str, dst: &mut [TChar]) {
    let mut len = 0;
    for (s, d) in src.encode_utf16().zip(dst.iter_mut()) {
        *d = s as TChar;
        len += 1;
    }
    if len < dst.len() {
        dst[len] = 0;
    } else if let Some(last) = dst.last_mut() {
        *last = 0;
    }
}

unsafe fn read_wstring(s: *const TChar) -> String {
    if s.is_null() {
        return String::new();
    }
    // Hosts pass a String128 here; cap the scan so an unterminated buffer
    // cannot send us walking off the end of it.
    let mut len = 0isize;
    while len < 128 && *s.offset(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(s.cast(), len as usize))
}

/// VST3 parameters are always normalised to 0..1, while Skuiz parameters
/// carry their real range, so every value crossing this boundary is
/// converted in both directions.
fn to_normalized(def: &ParamDef, plain: f64) -> f64 {
    let (lo, hi) = (def.low(), def.high());
    if hi <= lo {
        return 0.0;
    }
    ((plain - lo) / (hi - lo)).clamp(0.0, 1.0)
}

fn from_normalized(def: &ParamDef, normalized: f64) -> f64 {
    let (lo, hi) = (def.low(), def.high());
    let plain = lo + normalized.clamp(0.0, 1.0) * (hi - lo);
    // A discrete parameter must land exactly on an index, or the choice
    // label lookup falls through to a number.
    if def.choices.is_empty() {
        plain
    } else {
        plain.round()
    }
}

/// Derive a stable VST3 class id from the plugin id.
///
/// Hosts key saved projects on this, so it must never change for a given
/// plugin. Deriving it from the (already unique, already stable) plugin id
/// is more reliable than asking authors to mint and hand-copy a GUID.
pub const fn derive_cid(plugin_id: &str) -> TUID {
    // FNV-1a, run over the id with four different offset bases to fill 16
    // bytes. Plugin ids are reverse-DNS and already unique between
    // vendors; this only has to avoid accidental collisions, not resist
    // an attacker.
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    const BASES: [u64; 4] = [
        0xcbf2_9ce4_8422_2325,
        0x9e37_79b9_7f4a_7c15,
        0xff51_afd7_ed55_8ccd,
        0xc4ce_b9fe_1a85_ec53,
    ];

    let bytes = plugin_id.as_bytes();
    let mut out = [0u8; 16];
    let mut word = 0;
    while word < 4 {
        let mut hash = BASES[word];
        let mut i = 0;
        while i < bytes.len() {
            hash ^= bytes[i] as u64;
            hash = hash.wrapping_mul(PRIME);
            i += 1;
        }
        let h = hash.to_be_bytes();
        let mut b = 0;
        while b < 4 {
            // Fold the 64-bit hash down to the 4 bytes this word owns.
            out[word * 4 + b] = h[b] ^ h[b + 4];
            b += 1;
        }
        word += 1;
    }

    let mut tuid = [0 as c_char; 16];
    let mut i = 0;
    while i < 16 {
        tuid[i] = out[i] as c_char;
        i += 1;
    }
    tuid
}

// --- the plugin object --------------------------------------------------

/// State the plugin object and its editor view both need. Held behind an
/// `Arc` so the view cannot outlive it, and so a bus callback never points
/// at memory the host has freed.
struct Shared<P: Processor> {
    /// The engine owns the processor and everything realtime (see
    /// docs/concepts/invariants.md); Arc'd for the view and the bus
    /// callback's Weak handle.
    engine: Arc<Engine<P>>,
    /// Proof the engine is in the AUDIO state, stashed between
    /// `setProcessing(true)` and `setProcessing(false)` (or created on the
    /// fly by `process` for hosts that skip the call). Audio-thread only.
    audio_token: std::cell::RefCell<Option<AudioToken>>,
    /// Timed parameter points collected from the host for the current
    /// block: `(sample_offset, param_id, plain_value)`. Audio-thread only;
    /// pre-allocated so `process` never allocates. The block is split at
    /// these offsets so automation lands sample-accurately.
    param_events: UnsafeCell<Vec<(u32, u32, f64)>>,
    bus: Mutex<Option<skuiz_ipc::Bus>>,
    /// Last-writer-wins versions for shared parameters (invariant 9); bus
    /// and UI threads only.
    lww: Arc<skuiz_core::lww::Lww>,
    /// Host-driven activation state for the declared audio buses, by
    /// declaration index. `activateBus` is a main-thread call; `process`
    /// reads these on the audio thread. Only optional buses honor them —
    /// a non-optional bus is always active.
    input_bus_active: [AtomicBool; MAX_BUSES_PER_DIRECTION],
    output_bus_active: [AtomicBool; MAX_BUSES_PER_DIRECTION],
    /// The host's handler, retained while it is set. Editor-driven changes
    /// go through this, or the host never learns the GUI moved a parameter.
    handler: Mutex<Option<ComPtr<IComponentHandler>>>,
    /// Remote (bus/state) changes landed since the host was last told.
    /// Consumed in `getParamNormalized`, where the dirty flag becomes a
    /// `restartComponent(kParamValuesChanged)` — VST3 has no equivalent of
    /// CLAP's `request_callback`, and that getter is the closest thing to a
    /// regular main-thread entry point the interface offers.
    editor_dirty: AtomicBool,
}

impl<P: Processor> Shared<P> {
    /// Apply an editor-driven parameter change: update the processor (via
    /// the engine — direct when stopped, queued for the next block when
    /// running), tell the host so it records automation, and share it with
    /// other instances.
    fn set_param_from_editor(&self, id: u32, value: f64) {
        let Some(def) = P::params().iter().find(|p| p.id == id) else {
            return;
        };
        // For shared parameters the apply happens inside `stamp_with`: only
        // a change that entered the engine claims a version and reaches the
        // bus, so a dropped command never splits the instance from its
        // peers (invariant 9). Only parameters declared shared leave the
        // instance (invariant 10).
        let stamped = if def.shared {
            self.lww.stamp_with(id, || self.engine.set_param(id, value))
        } else {
            self.engine.set_param(id, value);
            None
        };
        // Clone the retained handler out and release the lock before the
        // COM calls: hosts may re-enter the controller synchronously from
        // performEdit, and holding our lock across that is a deadlock
        // waiting for a host to trigger it.
        let handler = self
            .handler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(handler) = handler {
            // VST3 expects a gesture around every edit; without it hosts
            // may ignore the value or fail to record automation.
            let normalized = to_normalized(def, value);
            unsafe {
                handler.beginEdit(id);
                handler.performEdit(id, normalized);
                handler.endEdit(id);
            }
        }
        // The versioned frame lets receivers discard stale echoes
        // (invariant 9); `None` means the change never entered the engine,
        // so nothing is broadcast.
        if let Some((seq, origin)) = stamped {
            if let Ok(bus) = self.bus.lock() {
                if let Some(bus) = bus.as_ref() {
                    bus.send(
                        skuiz_core::protocol::set_param_versioned(id, value, seq, origin)
                            .as_bytes(),
                    );
                }
            }
        }
    }
}

/// One Skuiz processor presented to a host as a VST3 single-component plugin.
pub struct Vst3Plugin<P: Processor> {
    shared: Arc<Shared<P>>,
    _marker: PhantomData<P>,
}

impl<P: Processor + Default> Default for Vst3Plugin<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: Processor + Default> Vst3Plugin<P> {
    /// Create an instance with every parameter at its default.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                engine: Engine::new(MIDI_OUT_CAPACITY),
                audio_token: std::cell::RefCell::new(None),
                param_events: UnsafeCell::new(Vec::with_capacity(PARAM_EVENT_CAPACITY)),
                bus: Mutex::new(None),
                lww: Arc::new(skuiz_core::lww::Lww::new()),
                input_bus_active: std::array::from_fn(|_| AtomicBool::new(false)),
                output_bus_active: std::array::from_fn(|_| AtomicBool::new(false)),
                handler: Mutex::new(None),
                editor_dirty: AtomicBool::new(false),
            }),
            _marker: PhantomData,
        }
    }
}

impl<P: Processor> Vst3Plugin<P> {
    /// Apply a parameter change as though it came from the editor: update
    /// the processor, report the edit to the host, and share it with other
    /// instances. This is exactly what the webview's IPC handler calls, and
    /// is public so it can be driven without a window.
    pub fn editor_edit(&self, id: u32, value: f64) {
        self.shared.set_param_from_editor(id, value);
    }

    fn param(index: i32) -> Option<&'static ParamDef> {
        P::params().get(index as usize)
    }

    fn param_by_id(id: u32) -> Option<&'static ParamDef> {
        P::params().iter().find(|p| p.id == id)
    }

    /// The declared audio buses in one direction, in declaration order.
    fn bus_specs(dir: CoreBusDirection) -> impl Iterator<Item = &'static AudioBusSpec> {
        P::audio_buses().iter().filter(move |s| s.direction == dir)
    }

    /// The VST3 speaker arrangement matching a declared layout.
    fn arrangement_of(layout: ChannelLayout) -> SpeakerArrangement {
        match layout {
            ChannelLayout::Mono => SpeakerArr::kMono,
            ChannelLayout::Stereo => SpeakerArr::kStereo,
            // No named arrangement: the first n speaker bits, which is how
            // the SDK builds every n-channel arrangement.
            ChannelLayout::Discrete(n) => (1 << n) - 1,
        }
    }

    /// Whether the host has this bus active. `activateBus` is a main-thread
    /// call; the audio thread reads the recorded state here. A non-optional
    /// bus is always active, whatever the host asked for.
    fn bus_is_active(&self, dir: CoreBusDirection, index: usize, spec: &AudioBusSpec) -> bool {
        if !spec.optional {
            return true;
        }
        let slots = match dir {
            CoreBusDirection::Input => &self.shared.input_bus_active,
            CoreBusDirection::Output => &self.shared.output_bus_active,
        };
        slots.get(index).is_some_and(|s| s.load(Ordering::Acquire))
    }

    /// Collect the block's timed parameter points into `out`
    /// (pre-allocated, never grows past [`PARAM_EVENT_CAPACITY`]). VST3
    /// sends a queue of timed points per parameter; every point is kept,
    /// converted to a plain value, clamped into the block, and sorted by
    /// sample offset so the block can be split at event times. Overflow
    /// policy (invariant 8): drop the excess points, count each one.
    unsafe fn collect_param_changes(
        &self,
        changes: *mut IParameterChanges,
        frames: usize,
        out: &mut Vec<(u32, u32, f64)>,
    ) {
        let Some(changes) = ComRef::from_raw(changes) else {
            return;
        };
        for i in 0..changes.getParameterCount() {
            let Some(queue) = ComRef::from_raw(changes.getParameterData(i)) else {
                continue;
            };
            let id = queue.getParameterId();
            let Some(def) = Self::param_by_id(id) else {
                continue;
            };
            for point in 0..queue.getPointCount() {
                if out.len() >= out.capacity() {
                    DiagCounters::bump(&self.shared.engine.diag().param_events_dropped);
                    continue;
                }
                let mut offset = 0;
                let mut normalized = 0.0;
                if queue.getPoint(point, &mut offset, &mut normalized) == kResultTrue {
                    let frame = (offset.max(0) as usize).min(frames) as u32;
                    out.push((frame, id, from_normalized(def, normalized)));
                }
            }
        }
        // Points across queues are not mutually ordered; sort them.
        out.sort_unstable_by_key(|e| e.0);
    }

    /// Convert generated MIDI into VST3 events. MIDI 1.0 note on/off and
    /// poly pressure map to native event types; CC, pitch bend and channel
    /// pressure travel as `kLegacyMIDICCOutEvent`. MIDI 2.0 UMP events are
    /// dropped: VST3 has no UMP event type, and a MIDI 2.0 note's attribute
    /// data would be lost in a lossy conversion — silent-and-documented
    /// beats wrong.
    /// `offset` is the segment's start within the block (the block is split
    /// at parameter-point times, so the DSP's frame numbers are
    /// segment-relative); `frames` is the whole block.
    unsafe fn emit_events(&self, out: *mut IEventList, midi: &MidiOut, frames: usize, offset: u32) {
        let Some(list) = ComRef::from_raw(out) else {
            return;
        };
        for &(frame, ev) in midi.events() {
            let Some(bytes) = ev.midi1_bytes() else {
                continue;
            };
            let status = bytes[0] & 0xF0;
            let channel = (bytes[0] & 0x0F) as i16;
            let mut event: Event = std::mem::zeroed();
            event.busIndex = 0;
            // The DSP is trusted for content, not for timing: an offset past
            // the end of the block is clamped rather than handed to the host.
            event.sampleOffset = (frame + offset).min(frames.saturating_sub(1) as u32) as i32;
            match status {
                0x90 if bytes[2] > 0 => {
                    event.r#type = Event_::EventTypes_::kNoteOnEvent as u16;
                    event.__field0.noteOn = NoteOnEvent {
                        channel,
                        pitch: bytes[1] as i16,
                        tuning: 0.0,
                        velocity: bytes[2] as f32 / 127.0,
                        length: 0,
                        noteId: -1,
                    };
                }
                // A note-on at velocity 0 is a note-off by MIDI convention.
                0x80 | 0x90 => {
                    event.r#type = Event_::EventTypes_::kNoteOffEvent as u16;
                    event.__field0.noteOff = NoteOffEvent {
                        channel,
                        pitch: bytes[1] as i16,
                        velocity: bytes[2] as f32 / 127.0,
                        noteId: -1,
                        tuning: 0.0,
                    };
                }
                0xA0 => {
                    // Poly pressure has a native VST3 event type.
                    event.r#type = Event_::EventTypes_::kPolyPressureEvent as u16;
                    event.__field0.polyPressure = PolyPressureEvent {
                        channel,
                        pitch: bytes[1] as i16,
                        pressure: bytes[2] as f32 / 127.0,
                        noteId: -1,
                    };
                }
                0xB0 | 0xD0 | 0xE0 => {
                    // CC as-is; channel pressure as kAfterTouch; pitch bend
                    // as kPitchBend with value = LSB, value2 = MSB.
                    let (control_number, value, value2) = match status {
                        0xB0 => (bytes[1], bytes[2], 0),
                        0xD0 => (ControllerNumbers_::kAfterTouch as u8, bytes[1], 0),
                        _ => (ControllerNumbers_::kPitchBend as u8, bytes[1], bytes[2]),
                    };
                    event.r#type = Event_::EventTypes_::kLegacyMIDICCOutEvent as u16;
                    event.__field0.midiCCOut = LegacyMIDICCOutEvent {
                        controlNumber: control_number,
                        // int8 is `c_char` in the bindings: signedness varies
                        // by target, so cast through the alias, not i8/u8.
                        channel: channel as std::ffi::c_char,
                        value: value as std::ffi::c_char,
                        value2: value2 as std::ffi::c_char,
                    };
                }
                _ => continue,
            }
            list.addEvent(&mut event);
        }
    }

    /// The pair of `setupProcessing`'s `activate`: the host is tearing the
    /// processing state down. Activation itself stays in `setupProcessing`,
    /// which is where the sample rate arrives.
    fn set_active_inner(&self, state: TBool) -> tresult {
        if state == 0 {
            self.shared
                .engine
                .with_main(|core| core.processor.deactivate());
        }
        kResultOk
    }

    /// The whole stream is read before the processor sees any of it:
    /// stream I/O is unbounded and must not run under the access protocol.
    unsafe fn set_state_inner(&self, state: *mut IBStream) -> tresult {
        let Some(stream) = ComRef::from_raw(state) else {
            return kInvalidArgument;
        };
        let mut data = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let mut read = 0i32;
            let r = stream.read(
                chunk.as_mut_ptr() as *mut c_void,
                chunk.len() as i32,
                &mut read,
            );
            if r != kResultOk && r != kResultTrue {
                return kResultFalse;
            }
            if read <= 0 {
                break;
            }
            data.extend_from_slice(&chunk[..read as usize]);
        }
        if self.shared.engine.load_state(data) {
            // Project state replaced the parameter values without versions:
            // stop advertising them to the bus until a shared edit lands
            // (invariant 10).
            self.shared.lww.on_state_load();
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn get_state_inner(&self, state: *mut IBStream) -> tresult {
        let Some(stream) = ComRef::from_raw(state) else {
            return kInvalidArgument;
        };
        // Direct when stopped, a bounded audio-thread round-trip when
        // running; None means the round-trip timed out.
        let Some(data) = self.shared.engine.save_state() else {
            return kResultFalse;
        };
        let mut written_total = 0usize;
        while written_total < data.len() {
            let mut written = 0i32;
            let r = stream.write(
                data[written_total..].as_ptr() as *mut c_void,
                (data.len() - written_total) as i32,
                &mut written,
            );
            if (r != kResultOk && r != kResultTrue) || written <= 0 {
                return kResultFalse;
            }
            written_total += written as usize;
        }
        kResultOk
    }

    fn setup_processing_inner(&self, setup: *mut ProcessSetup) -> tresult {
        // SAFETY: caller (the trait wrapper) guarantees the host's pointer.
        let Some(setup) = (unsafe { setup.as_ref() }) else {
            return kInvalidArgument;
        };
        // setupProcessing is main-thread with the transport stopped; a
        // host calling it while processing is breaking the spec, and we
        // say no rather than race.
        let Some(latency) = self.shared.engine.with_main(|core| {
            core.processor
                .activate(setup.sampleRate, setup.maxSamplesPerBlock as u32);
            core.processor.latency()
        }) else {
            return kResultFalse;
        };
        self.shared.engine.set_latency(latency);
        kResultOk
    }

    unsafe fn process_inner(&self, data: *mut ProcessData) -> tresult {
        let Some(data) = data.as_ref() else {
            return kInvalidArgument;
        };
        let frames = data.numSamples.max(0) as usize;
        let engine = &*self.shared.engine;

        // No locks below: the audio thread owns the processor (invariants
        // 1-2). Queued remote/editor/state changes apply at block top; host
        // automation lands sample-accurately in the segment loop.
        //
        // Hosts should call setProcessing(true) first; tolerate the ones
        // (and the tests) that don't, rather than render silence.
        if !engine.is_processing() {
            self.shared.audio_token.replace(Some(engine.begin_audio()));
        }
        let core = {
            let token = self.shared.audio_token.borrow();
            engine.audio_core(token.as_ref().expect("AUDIO implies a token"))
        };
        let report = engine.drain_commands(core);
        if report.notify_main {
            self.shared.editor_dirty.store(true, Ordering::Release);
        }
        // The block's timed automation points, sorted by sample offset.
        let events = &mut *self.shared.param_events.get();
        events.clear();
        self.collect_param_changes(data.inputParameterChanges, frames, events);

        // Copy the main input into the main output once (unless the host
        // processes in place) and zero what no input covered; segments
        // then re-slice those buffers, and any sidechain input is read
        // directly from the host's buffers.
        self.copy_main_into_output(data, frames);

        // Split the block at point times: render up to the next point,
        // apply it, continue. Segments re-slice the same host buffers.
        let mut pos = 0usize;
        for &(time, id, value) in events.iter() {
            let t = (time as usize).min(frames);
            if t > pos {
                self.process_segment(core, data, pos, t);
                pos = t;
            }
            core.processor.set_param(id, value);
            // Publish the readback, not the request: processors may round
            // or clamp, and the mirror must hold the value actually in
            // force.
            engine.mirror().publish(id, core.processor.get_param(id));
        }
        if pos < frames {
            self.process_segment(core, data, pos, frames);
        }
        kResultOk
    }

    /// Copy the main input into the main output (unless the host processes
    /// in place) and zero any main-output channels no input landed in.
    /// Runs once per block; segments then re-slice those buffers. Without
    /// this, an absent input would feed stale data from an earlier block
    /// to the DSP.
    // Channel loops index raw host pointers (`ptrs.add(c)`), so an index
    // loop is the honest shape here.
    #[allow(clippy::needless_range_loop)]
    unsafe fn copy_main_into_output(&self, data: &ProcessData, frames: usize) {
        let Some(out_spec) = Self::bus_specs(CoreBusDirection::Output).next() else {
            return;
        };
        if data.numOutputs < 1 || data.outputs.is_null() {
            return;
        }
        let out_bus = &*data.outputs;
        let out_ptrs = out_bus.__field0.channelBuffers32;
        if out_ptrs.is_null() {
            return;
        }
        let n_out = (out_bus.numChannels.max(0) as usize).min(out_spec.layout.channels() as usize);
        let mut copied = 0;
        if Self::bus_specs(CoreBusDirection::Input).next().is_some()
            && data.numInputs >= 1
            && !data.inputs.is_null()
        {
            let in_bus = &*data.inputs;
            let in_ptrs = in_bus.__field0.channelBuffers32;
            if !in_ptrs.is_null() {
                copied = n_out.min(in_bus.numChannels.max(0) as usize);
                for c in 0..copied {
                    let src = *in_ptrs.add(c);
                    let dst = *out_ptrs.add(c);
                    if !src.is_null() && !dst.is_null() && src != dst {
                        std::ptr::copy_nonoverlapping(src, dst, frames);
                    }
                }
            }
        }
        // Channels no input was copied into — every channel when the host
        // connects none — must be silenced, not left holding whatever the
        // buffer already contained.
        for c in copied..n_out {
            let dst = *out_ptrs.add(c);
            if !dst.is_null() {
                std::ptr::write_bytes(dst, 0, frames);
            }
        }
    }

    /// Render one segment of a block: repoint the engine's bus scratch at
    /// `buffer[start..end]` per channel and run the processor. The main
    /// input aliases the main output (copy-in landed there); sidechain
    /// inputs read the host's buffers directly and are active only when
    /// the host activated and connected them. MIDI queued by the DSP
    /// carries segment-relative frame numbers, so it is emitted with
    /// `start` added back.
    // Channel loops index raw host pointers (`ptrs.add(c)`), so an index
    // loop is the honest shape here.
    #[allow(clippy::needless_range_loop)]
    unsafe fn process_segment(
        &self,
        core: &mut AudioCore<P>,
        data: &ProcessData,
        start: usize,
        end: usize,
    ) {
        let len = end - start;
        let scratch = &mut core.bus_scratch;
        scratch.clear();

        // Outputs: each declared bus maps to the host bus of the same
        // index. A host that connects no outputs still gets the processor
        // run with the bus inactive, so MIDI-only DSP keeps running.
        let mut main_out: [*mut f32; MAX_BUS_CHANNELS] = [std::ptr::null_mut(); MAX_BUS_CHANNELS];
        let mut main_out_n = 0usize;
        for (i, spec) in Self::bus_specs(CoreBusDirection::Output).enumerate() {
            if i >= data.numOutputs.max(0) as usize || data.outputs.is_null() {
                continue;
            }
            let bus = &*data.outputs.add(i);
            let ptrs = bus.__field0.channelBuffers32;
            if ptrs.is_null() || !self.bus_is_active(CoreBusDirection::Output, i, spec) {
                continue;
            }
            scratch.set_active(CoreBusDirection::Output, i, true);
            let n = (bus.numChannels.max(0) as usize).min(spec.layout.channels() as usize);
            for c in 0..n {
                let ptr = *ptrs.add(c);
                if ptr.is_null() {
                    break;
                }
                scratch.set_channel(CoreBusDirection::Output, i, c, ptr.add(start), len);
                if i == 0 {
                    // Segment-offset pointer, reused verbatim for the main
                    // input alias below.
                    main_out[c] = ptr.add(start);
                    main_out_n = c + 1;
                }
            }
        }

        // Inputs: the main bus aliases the main output; any further input
        // (a sidechain) reads the host's buffer directly, active only when
        // the host activated and connected it.
        for (i, spec) in Self::bus_specs(CoreBusDirection::Input).enumerate() {
            if i == 0 {
                if main_out_n == 0 {
                    continue;
                }
                scratch.set_active(CoreBusDirection::Input, 0, true);
                for (c, ptr) in main_out.iter().enumerate().take(main_out_n) {
                    scratch.set_channel(CoreBusDirection::Input, 0, c, *ptr, len);
                }
                continue;
            }
            if !self.bus_is_active(CoreBusDirection::Input, i, spec) {
                continue;
            }
            if i >= data.numInputs.max(0) as usize || data.inputs.is_null() {
                continue;
            }
            let bus = &*data.inputs.add(i);
            let ptrs = bus.__field0.channelBuffers32;
            if ptrs.is_null() {
                continue;
            }
            scratch.set_active(CoreBusDirection::Input, i, true);
            let n = (bus.numChannels.max(0) as usize).min(spec.layout.channels() as usize);
            for c in 0..n {
                let ptr = *ptrs.add(c);
                if ptr.is_null() {
                    break;
                }
                scratch.set_channel(CoreBusDirection::Input, i, c, ptr.add(start), len);
            }
        }

        let (inputs, mut outputs) = scratch.views();
        let midi = &mut core.midi_out;
        midi.clear();
        core.processor.process(&inputs, &mut outputs, midi);
        // MIDI pushed past capacity is a counted drop (invariant 8).
        if midi.dropped() > 0 {
            self.shared
                .engine
                .diag()
                .midi_events_dropped
                .fetch_add(midi.dropped() as u64, Ordering::Relaxed);
        }
        self.emit_events(
            data.outputEvents,
            midi,
            data.numSamples.max(0) as usize,
            start as u32,
        );
    }

    fn get_param_normalized_inner(&self, id: u32) -> f64 {
        // If remote (bus/state) changes landed since the last refresh, tell
        // the host to re-query everything. `restartComponent` is the VST3
        // mechanism for "parameter values changed without an edit"; it must
        // run on the main thread, and this getter is as close to a regular
        // main-thread entry as the interface offers — hosts poll it for
        // their generic editors and automation lanes. The flag is consumed
        // before the call, so a host answering the restart synchronously
        // cannot recurse.
        if self.shared.editor_dirty.swap(false, Ordering::Acquire) {
            let handler = self
                .shared
                .handler
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(handler) = handler {
                unsafe {
                    handler.restartComponent(RestartFlags_::kParamValuesChanged);
                }
            }
        }
        // A runtime latency change (flagged by the engine's per-block poll)
        // is announced the same way, with its own restart flag.
        if self.shared.engine.take_latency_changed() {
            let handler = self
                .shared
                .handler
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(handler) = handler {
                unsafe {
                    handler.restartComponent(RestartFlags_::kLatencyChanged);
                }
            }
        }
        let Some(def) = Self::param_by_id(id) else {
            return 0.0;
        };
        // The mirror answers host reads wait-free (invariant 6); a param the
        // mirror doesn't know is not ours.
        match self.shared.engine.mirror().get(id) {
            Some(plain) => to_normalized(def, plain),
            None => 0.0,
        }
    }

    fn set_param_normalized_inner(&self, id: u32, value: f64) -> tresult {
        let Some(def) = Self::param_by_id(id) else {
            return kInvalidArgument;
        };
        // Host-driven change: direct when stopped, queued for the next block
        // when running; never locks the processor.
        self.shared
            .engine
            .set_param(id, from_normalized(def, value));
        kResultOk
    }
}

impl<P: Processor + Default> Class for Vst3Plugin<P> {
    type Interfaces = (IComponent, IAudioProcessor, IEditController);
}

impl<P: Processor + Default> IPluginBaseTrait for Vst3Plugin<P> {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        use skuiz_core::protocol as proto;
        // Join the IPC bus, exactly as the CLAP adapter does, so instances
        // share state across formats and processes alike. The callback holds
        // only an engine handle (Weak), so a frame arriving after this
        // instance is gone goes nowhere.
        let handle = self.shared.engine.handle();
        let lww = Arc::clone(&self.shared.lww);
        let lww_cb = Arc::clone(&lww);
        let sender_slot: Arc<Mutex<Option<skuiz_ipc::BusSender>>> = Arc::new(Mutex::new(None));
        let cb_sender = Arc::clone(&sender_slot);
        let bus = skuiz_ipc::Bus::join(P::info().id, move |frame| {
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
                // Local parameters never sync (invariant 10); stale versions
                // lose (invariant 9). The version is recorded only if the
                // change entered the engine, so a frame dropped by a full
                // command queue can still win when re-delivered.
                if !skuiz_core::syncs_over_bus::<P>(id) {
                    return;
                }
                lww_cb.accept_with(id, version, || handle.set_param(id, value));
                return;
            }
            if proto::parse_sync_request(msg).is_some() {
                // A late joiner asked for shared state; answer with the
                // parameters we hold a *fresh* version for — ones edited
                // over the bus and not rewritten by a project load since.
                // Never-edited and post-load parameters are omitted: their
                // value is host automation or project state, which is
                // per-instance (invariant 10). LWW makes duplicate answers
                // safe.
                let entries: Vec<(u32, f64, u64, u64)> = handle
                    .snapshot_params()
                    .into_iter()
                    .filter(|(id, _)| skuiz_core::syncs_over_bus::<P>(*id))
                    .filter_map(|(id, value)| {
                        lww_cb
                            .advertised_version(id)
                            .map(|(seq, origin)| (id, value, seq, origin))
                    })
                    .collect();
                if entries.is_empty() {
                    return;
                }
                if let Some(sender) = cb_sender.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    sender.send(proto::sync_state(&entries).as_bytes());
                }
                return;
            }
            if let Some(entries) = proto::parse_sync_state(msg) {
                for (id, value, seq, origin) in entries {
                    if skuiz_core::syncs_over_bus::<P>(id) {
                        lww_cb.accept_with(id, Some((seq, origin)), || handle.set_param(id, value));
                    }
                }
            }
        });
        *sender_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(bus.sender());
        // Late joiner: ask the bus for current shared state.
        bus.send(proto::sync_request(lww.origin()).as_bytes());
        *self.shared.bus.lock().unwrap_or_else(|e| e.into_inner()) = Some(bus);
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        *self.shared.bus.lock().unwrap_or_else(|e| e.into_inner()) = None;
        kResultOk
    }
}

impl<P: Processor + Default> IComponentTrait for Vst3Plugin<P> {
    unsafe fn getControllerClassId(&self, class_id: *mut TUID) -> tresult {
        // Single component: the controller *is* this object.
        *class_id = derive_cid(P::info().id);
        kResultOk
    }

    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }

    unsafe fn getBusCount(&self, media_type: MediaType, dir: BusDirection) -> i32 {
        match media_type as MediaTypes {
            MediaTypes_::kAudio => {
                let dir = if dir as BusDirections == BusDirections_::kInput {
                    CoreBusDirection::Input
                } else {
                    CoreBusDirection::Output
                };
                Self::bus_specs(dir).count() as i32
            }
            MediaTypes_::kEvent
                // Only advertise an event bus for plugins that generate MIDI.
                if P::emits_midi() && dir as BusDirections == BusDirections_::kOutput => {
                    1
                }
            _ => 0,
        }
    }

    unsafe fn getBusInfo(
        &self,
        media_type: MediaType,
        dir: BusDirection,
        index: i32,
        bus: *mut BusInfo,
    ) -> tresult {
        if index < 0 || bus.is_null() {
            return kInvalidArgument;
        }
        let is_input = dir as BusDirections == BusDirections_::kInput;
        let bus = &mut *bus;
        match media_type as MediaTypes {
            MediaTypes_::kAudio => {
                let spec_dir = if is_input {
                    CoreBusDirection::Input
                } else {
                    CoreBusDirection::Output
                };
                let Some(spec) = Self::bus_specs(spec_dir).nth(index as usize) else {
                    return kInvalidArgument;
                };
                bus.mediaType = MediaTypes_::kAudio as MediaType;
                bus.direction = dir;
                bus.channelCount = spec.layout.channels() as i32;
                copy_wstring(spec.name, &mut bus.name);
                // The first bus of a direction is the main bus; any further
                // input is an aux (a sidechain).
                bus.busType = if index == 0 {
                    BusTypes_::kMain as BusType
                } else {
                    BusTypes_::kAux as BusType
                };
                // The cast is platform-load-bearing: these enum constants
                // are i32 on Windows and u32 on macOS, so clippy sees a
                // same-type cast on one platform that the other requires.
                #[allow(clippy::unnecessary_cast)]
                {
                    bus.flags = if spec.optional {
                        0
                    } else {
                        BusInfo_::BusFlags_::kDefaultActive as u32
                    };
                }
                kResultOk
            }
            MediaTypes_::kEvent if index == 0 && P::emits_midi() && !is_input => {
                bus.mediaType = MediaTypes_::kEvent as MediaType;
                bus.direction = dir;
                bus.channelCount = 16;
                copy_wstring("MIDI Out", &mut bus.name);
                bus.busType = BusTypes_::kMain as BusType;
                // The cast is platform-load-bearing: these enum constants
                // are i32 on Windows and u32 on macOS, so clippy sees a
                // same-type cast on one platform that the other requires.
                #[allow(clippy::unnecessary_cast)]
                {
                    bus.flags = BusInfo_::BusFlags_::kDefaultActive as u32;
                }
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn getRoutingInfo(&self, _i: *mut RoutingInfo, _o: *mut RoutingInfo) -> tresult {
        kNotImplemented
    }

    unsafe fn activateBus(
        &self,
        media_type: MediaType,
        dir: BusDirection,
        index: i32,
        state: TBool,
    ) -> tresult {
        // Event buses carry no activation state worth recording.
        if media_type as MediaTypes != MediaTypes_::kAudio || index < 0 {
            return kResultOk;
        }
        let (spec_dir, slots) = if dir as BusDirections == BusDirections_::kInput {
            (CoreBusDirection::Input, &self.shared.input_bus_active)
        } else {
            (CoreBusDirection::Output, &self.shared.output_bus_active)
        };
        if Self::bus_specs(spec_dir).nth(index as usize).is_none() {
            return kInvalidArgument;
        }
        // Main thread here, audio thread in `process` — the atomic is the
        // whole handshake. A non-optional bus ignores the recorded state.
        slots[index as usize].store(state != 0, Ordering::Release);
        kResultOk
    }

    unsafe fn setActive(&self, state: TBool) -> tresult {
        ffi_guard(|| self.set_active_inner(state), kResultFalse)
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        ffi_guard(|| self.set_state_inner(state), kResultFalse)
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        ffi_guard(|| self.get_state_inner(state), kResultFalse)
    }
}

impl<P: Processor + Default> IAudioProcessorTrait for Vst3Plugin<P> {
    unsafe fn setBusArrangements(
        &self,
        inputs: *mut SpeakerArrangement,
        num_ins: i32,
        outputs: *mut SpeakerArrangement,
        num_outs: i32,
    ) -> tresult {
        let n_in = Self::bus_specs(CoreBusDirection::Input).count() as i32;
        let n_out = Self::bus_specs(CoreBusDirection::Output).count() as i32;
        // The topology is fixed at build time: the host must offer exactly
        // the declared bus counts, and each arrangement must match the
        // declared layout (kEmpty only for an optional bus it deactivated).
        if num_ins != n_in
            || num_outs != n_out
            || (num_ins > 0 && inputs.is_null())
            || (num_outs > 0 && outputs.is_null())
        {
            return kResultFalse;
        }
        let matches = |arrs: *mut SpeakerArrangement, dir: CoreBusDirection| {
            Self::bus_specs(dir).enumerate().all(|(i, spec)| {
                let arr = *arrs.add(i);
                (arr == SpeakerArr::kEmpty && spec.optional)
                    || arr == Self::arrangement_of(spec.layout)
            })
        };
        if matches(inputs, CoreBusDirection::Input) && matches(outputs, CoreBusDirection::Output) {
            kResultTrue
        } else {
            kResultFalse
        }
    }

    unsafe fn getBusArrangement(
        &self,
        dir: BusDirection,
        index: i32,
        arr: *mut SpeakerArrangement,
    ) -> tresult {
        if index < 0 || arr.is_null() {
            return kInvalidArgument;
        }
        let spec_dir = if dir as BusDirections == BusDirections_::kInput {
            CoreBusDirection::Input
        } else {
            CoreBusDirection::Output
        };
        let Some(spec) = Self::bus_specs(spec_dir).nth(index as usize) else {
            return kInvalidArgument;
        };
        *arr = Self::arrangement_of(spec.layout);
        kResultOk
    }

    unsafe fn canProcessSampleSize(&self, size: i32) -> tresult {
        // 64-bit processing would need a second code path through the
        // Processor trait; hosts all support 32-bit.
        if size as SymbolicSampleSizes == SymbolicSampleSizes_::kSample32 {
            kResultOk
        } else {
            kNotImplemented
        }
    }

    unsafe fn getLatencySamples(&self) -> u32 {
        self.shared.engine.latency()
    }

    unsafe fn setupProcessing(&self, setup: *mut ProcessSetup) -> tresult {
        ffi_guard(|| self.setup_processing_inner(setup), kResultFalse)
    }

    unsafe fn setProcessing(&self, state: TBool) -> tresult {
        if state != 0 {
            self.shared
                .audio_token
                .replace(Some(self.shared.engine.begin_audio()));
        } else if let Some(token) = self.shared.audio_token.take() {
            self.shared.engine.end_audio(token);
        }
        kResultOk
    }

    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        ffi_guard(|| self.process_inner(data), kResultFalse)
    }

    unsafe fn getTailSamples(&self) -> u32 {
        0
    }
}

impl<P: Processor + Default> IEditControllerTrait for Vst3Plugin<P> {
    unsafe fn setComponentState(&self, state: *mut IBStream) -> tresult {
        // Single component: the same object already holds this state, so
        // route to IComponent's implementation rather than the (no-op)
        // IEditController one.
        IComponentTrait::setState(self, state)
    }

    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getParameterCount(&self) -> i32 {
        P::params().len() as i32
    }

    unsafe fn getParameterInfo(&self, index: i32, info: *mut ParameterInfo) -> tresult {
        let (Some(def), false) = (Self::param(index), info.is_null()) else {
            return kInvalidArgument;
        };
        let info = &mut *info;
        info.id = def.id;
        copy_wstring(def.name, &mut info.title);
        copy_wstring(def.name, &mut info.shortTitle);
        copy_wstring("", &mut info.units);
        // A discrete parameter reports its step count so the host draws a
        // stepped control (and a dropdown when it also has a list).
        info.stepCount = if def.choices.is_empty() {
            0
        } else {
            def.choices.len() as i32 - 1
        };
        info.defaultNormalizedValue = to_normalized(def, def.default);
        info.unitId = 0;
        info.flags = ParameterInfo_::ParameterFlags_::kCanAutomate
            | if def.choices.is_empty() {
                0
            } else {
                ParameterInfo_::ParameterFlags_::kIsList
            };
        kResultOk
    }

    unsafe fn getParamStringByValue(
        &self,
        id: u32,
        value_normalized: f64,
        string: *mut String128,
    ) -> tresult {
        let (Some(def), false) = (Self::param_by_id(id), string.is_null()) else {
            return kInvalidArgument;
        };
        let plain = from_normalized(def, value_normalized);
        let text = match def.label(plain) {
            Some(label) => label.to_string(),
            None => format!("{plain:.3}"),
        };
        copy_wstring(&text, &mut *string);
        kResultOk
    }

    unsafe fn getParamValueByString(
        &self,
        id: u32,
        string: *mut TChar,
        value_normalized: *mut f64,
    ) -> tresult {
        let (Some(def), false) = (Self::param_by_id(id), value_normalized.is_null()) else {
            return kInvalidArgument;
        };
        let text = read_wstring(string);
        let text = text.trim();
        // Accept a choice label as well as a number, matching CLAP.
        if let Some(idx) = def.choices.iter().position(|c| *c == text) {
            *value_normalized = to_normalized(def, idx as f64);
            return kResultOk;
        }
        match text.parse::<f64>() {
            Ok(plain) => {
                *value_normalized = to_normalized(def, plain);
                kResultOk
            }
            Err(_) => kInvalidArgument,
        }
    }

    unsafe fn normalizedParamToPlain(&self, id: u32, value_normalized: f64) -> f64 {
        Self::param_by_id(id).map_or(0.0, |def| from_normalized(def, value_normalized))
    }

    unsafe fn plainParamToNormalized(&self, id: u32, plain: f64) -> f64 {
        Self::param_by_id(id).map_or(0.0, |def| to_normalized(def, plain))
    }

    unsafe fn getParamNormalized(&self, id: u32) -> f64 {
        ffi_guard(|| self.get_param_normalized_inner(id), 0.0)
    }

    unsafe fn setParamNormalized(&self, id: u32, value: f64) -> tresult {
        ffi_guard(|| self.set_param_normalized_inner(id, value), kResultFalse)
    }

    unsafe fn setComponentHandler(&self, handler: *mut IComponentHandler) -> tresult {
        // Retain it: the editor reports its edits through this, and a null
        // handler means the host is taking it away.
        let retained = ComRef::from_raw(handler).map(|h| h.to_com_ptr());
        if let Ok(mut slot) = self.shared.handler.lock() {
            *slot = retained;
        }
        kResultOk
    }

    unsafe fn createView(&self, name: *const c_char) -> *mut IPlugView {
        // Hosts ask for named views; only the editor exists, and a plugin
        // without editor HTML has none at all. The spec defines exactly one
        // view name, "editor" — a null or foreign name gets nothing.
        if P::editor_html().is_none() || !EDITOR_SUPPORTED {
            return std::ptr::null_mut();
        }
        if name.is_null() || CStr::from_ptr(name) != CStr::from_ptr(ViewType::kEditor) {
            return std::ptr::null_mut();
        }
        let view = Vst3PlugView::<P> {
            shared: Arc::clone(&self.shared),
            editor: std::rc::Rc::new(std::cell::RefCell::new(None)),
        };
        match ComWrapper::new(view).to_com_ptr::<IPlugView>() {
            Some(ptr) => ptr.into_raw(),
            None => std::ptr::null_mut(),
        }
    }
}

/// The plugin's editor window: the same wry webview the CLAP adapter
/// embeds, attached to the NSView the host provides.
///
/// VST3 hands the plugin a parent view and expects the plugin to fill it,
/// which is exactly the contract `skuiz_ui::Editor` already implements for
/// CLAP, so both formats share one editor and one HTML file.
pub struct Vst3PlugView<P: Processor> {
    shared: Arc<Shared<P>>,
    /// Main-thread only: VST3 calls the view on the UI thread. Shared with
    /// the webview's IPC closure so it can answer queries with an eval.
    editor: std::rc::Rc<std::cell::RefCell<Option<skuiz_ui::Editor>>>,
}

impl<P: Processor + Default> Class for Vst3PlugView<P> {
    type Interfaces = (IPlugView,);
}

impl<P: Processor + Default> IPlugViewTrait for Vst3PlugView<P> {
    unsafe fn isPlatformTypeSupported(&self, type_: FIDString) -> tresult {
        if !type_.is_null() && CStr::from_ptr(type_) == CStr::from_ptr(native_platform_type()) {
            kResultTrue
        } else {
            kResultFalse
        }
    }

    unsafe fn attached(&self, parent: *mut c_void, type_: FIDString) -> tresult {
        if self.isPlatformTypeSupported(type_) != kResultTrue {
            return kInvalidArgument;
        }
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        {
            let Some(html) = P::editor_html() else {
                return kResultFalse;
            };
            #[cfg(target_os = "macos")]
            let view = skuiz_ui::ParentView::from_ns_view(parent);
            #[cfg(target_os = "windows")]
            let view = skuiz_ui::ParentView::from_hwnd(parent);
            // X11EmbedWindowID passes the window id as a pointer-sized value.
            #[cfg(target_os = "linux")]
            let view = skuiz_ui::ParentView::from_x11(parent as std::ffi::c_ulong);
            let Some(view) = view else {
                return kInvalidArgument;
            };
            let shared = Arc::clone(&self.shared);
            let editor_slot = std::rc::Rc::clone(&self.editor);
            let editor = skuiz_ui::Editor::attach(&view, html, P::editor_size(), move |msg| {
                if msg == skuiz_core::protocol::DIAG_QUERY {
                    // Diagnostics for the page: counters are atomics; the
                    // eval back is main-thread editor work.
                    let js = skuiz_core::protocol::on_diag_js(shared.engine.diag());
                    if let Some(editor) = editor_slot.borrow().as_ref() {
                        let _ = editor.eval(&js);
                    }
                    return;
                }
                let Some((id, value)) = skuiz_core::protocol::parse_set_param(&msg) else {
                    return;
                };
                shared.set_param_from_editor(id, value);
            });
            match editor {
                Ok(editor) => {
                    // Seed the page with the current values — read from the
                    // mirror (wait-free), never from the processor.
                    for (id, value) in self.shared.engine.mirror().snapshot() {
                        let _ = editor.eval(&skuiz_core::protocol::on_param_js(id, value));
                    }
                    *self.editor.borrow_mut() = Some(editor);
                    kResultOk
                }
                Err(_) => kResultFalse,
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            let _ = parent;
            kResultFalse
        }
    }

    unsafe fn removed(&self) -> tresult {
        *self.editor.borrow_mut() = None;
        kResultOk
    }

    // The webview handles its own input; returning false lets the host keep
    // its shortcuts working over the plugin window.
    unsafe fn onWheel(&self, _distance: f32) -> tresult {
        kResultFalse
    }

    unsafe fn onKeyDown(&self, _key: char16, _code: int16, _mods: int16) -> tresult {
        kResultFalse
    }

    unsafe fn onKeyUp(&self, _key: char16, _code: int16, _mods: int16) -> tresult {
        kResultFalse
    }

    unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
        let Some(size) = size.as_mut() else {
            return kInvalidArgument;
        };
        let (w, h) = P::editor_size();
        size.left = 0;
        size.top = 0;
        size.right = w as int32;
        size.bottom = h as int32;
        kResultOk
    }

    unsafe fn onSize(&self, new_size: *mut ViewRect) -> tresult {
        let Some(rect) = new_size.as_ref() else {
            return kInvalidArgument;
        };
        let width = (rect.right - rect.left).max(0) as u32;
        let height = (rect.bottom - rect.top).max(0) as u32;
        if let Some(editor) = self.editor.borrow().as_ref() {
            let _ = editor.resize((width, height));
        }
        kResultOk
    }

    unsafe fn onFocus(&self, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn setFrame(&self, _frame: *mut IPlugFrame) -> tresult {
        // Only needed to request a resize, which a fixed-size editor never
        // does, so the frame is accepted and not retained.
        kResultOk
    }

    unsafe fn canResize(&self) -> tresult {
        kResultFalse
    }

    unsafe fn checkSizeConstraint(&self, rect: *mut ViewRect) -> tresult {
        // Fixed size: force the host back to our dimensions.
        self.getSize(rect)
    }
}

/// The plugin factory. Generic over the processor so `export_vst3!` can
/// hand it the concrete type.
pub struct Vst3Factory<P: Processor + Default>(PhantomData<P>);

impl<P: Processor + Default> Default for Vst3Factory<P> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<P: Processor + Default> Class for Vst3Factory<P> {
    type Interfaces = (IPluginFactory,);
}

impl<P: Processor + Default> IPluginFactoryTrait for Vst3Factory<P> {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        let Some(info) = info.as_mut() else {
            return kInvalidArgument;
        };
        let plugin = P::info();
        copy_cstring(plugin.vendor, &mut info.vendor);
        copy_cstring("", &mut info.url);
        copy_cstring("", &mut info.email);
        info.flags = PFactoryInfo_::FactoryFlags_::kUnicode as int32;
        kResultOk
    }

    unsafe fn countClasses(&self) -> i32 {
        // One: this is a single-component plugin.
        1
    }

    unsafe fn getClassInfo(&self, index: i32, info: *mut PClassInfo) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        let Some(info) = info.as_mut() else {
            return kInvalidArgument;
        };
        let plugin = P::info();
        info.cid = derive_cid(plugin.id);
        info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
        copy_cstring("Audio Module Class", &mut info.category);
        copy_cstring(plugin.name, &mut info.name);
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: FIDString,
        iid: FIDString,
        obj: *mut *mut c_void,
    ) -> tresult {
        if cid.is_null() || obj.is_null() {
            return kInvalidArgument;
        }
        if *(cid as *const TUID) != derive_cid(P::info().id) {
            return kInvalidArgument;
        }
        let Some(instance) = ComWrapper::new(Vst3Plugin::<P>::new()).to_com_ptr::<FUnknown>()
        else {
            return kResultFalse;
        };
        let ptr = instance.as_ptr();
        ((*(*ptr).vtbl).queryInterface)(ptr, iid as *mut TUID, obj)
    }
}

/// Export `$P` as this cdylib's VST3 entry point.
#[macro_export]
macro_rules! export_vst3 {
    ($P:ty) => {
        const _: () = {
            use $crate::vst3::ComWrapper;
            use $crate::vst3::Steinberg::*;

            #[no_mangle]
            extern "system" fn GetPluginFactory() -> *mut IPluginFactory {
                ComWrapper::new($crate::Vst3Factory::<$P>::default())
                    .to_com_ptr::<IPluginFactory>()
                    .map(|p| p.into_raw())
                    .unwrap_or(::std::ptr::null_mut())
            }

            // macOS entry points per the SDK's macmain.cpp: lowerCamelCase,
            // and Steinberg's validator rejects the bundle without them.
            #[cfg(target_os = "macos")]
            #[no_mangle]
            extern "system" fn bundleEntry(_bundle: *mut ::std::ffi::c_void) -> bool {
                true
            }

            #[cfg(target_os = "macos")]
            #[no_mangle]
            extern "system" fn bundleExit() -> bool {
                true
            }

            #[cfg(target_os = "windows")]
            #[no_mangle]
            extern "system" fn InitDll() -> bool {
                true
            }

            #[cfg(target_os = "windows")]
            #[no_mangle]
            extern "system" fn ExitDll() -> bool {
                true
            }

            #[cfg(target_os = "linux")]
            #[no_mangle]
            extern "system" fn ModuleEntry(_handle: *mut ::std::ffi::c_void) -> bool {
                true
            }

            #[cfg(target_os = "linux")]
            #[no_mangle]
            extern "system" fn ModuleExit() -> bool {
                true
            }
        };
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cid_is_stable_and_distinct() {
        let a = derive_cid("org.skuiz.shared-gain");
        assert_eq!(a, derive_cid("org.skuiz.shared-gain"), "cid must be stable");
        assert_ne!(
            a,
            derive_cid("org.skuiz.trigger-note"),
            "different plugins collide"
        );
        assert_ne!(
            a,
            derive_cid("org.skuiz.shared-gai"),
            "near-miss ids collide"
        );
        assert!(a.iter().any(|b| *b != 0), "cid must not be all zeroes");
    }

    #[test]
    fn normalization_round_trips() {
        let cont = ParamDef {
            id: 0,
            name: "g",
            min: -6.0,
            max: 6.0,
            default: 0.0,
            choices: &[],
            shared: true,
        };
        assert_eq!(to_normalized(&cont, -6.0), 0.0);
        assert_eq!(to_normalized(&cont, 6.0), 1.0);
        assert_eq!(from_normalized(&cont, 0.5), 0.0);
        // out-of-range input must clamp, not extrapolate
        assert_eq!(to_normalized(&cont, 99.0), 1.0);
        assert_eq!(from_normalized(&cont, 9.0), 6.0);

        let choice = ParamDef {
            id: 1,
            name: "m",
            min: 0.0,
            max: 0.0,
            default: 0.0,
            choices: &["A", "B", "C"],
            shared: true,
        };
        // Every index must survive the trip through normalized space, or
        // hosts would land between choices.
        for i in 0..3 {
            let n = to_normalized(&choice, i as f64);
            assert_eq!(
                from_normalized(&choice, n),
                i as f64,
                "index {i} did not round-trip"
            );
        }
        // ...and an in-between normalized value must snap to an index.
        assert_eq!(from_normalized(&choice, 0.4), 1.0);
    }
}
