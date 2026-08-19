//! skuiz-vst3: VST3 format adapter.
//!
//! Implement [`skuiz_core::Processor`] (plus `Default`) and export it from a
//! `cdylib` with `skuiz_vst3::export_vst3!(MyProcessor);`.
//!
//! # Licensing
//!
//! Skuiz is MIT and stays that way: this adapter builds on the clean-room
//! MIT/Apache-2.0 `vst3` bindings, and no Steinberg SDK code is vendored or
//! linked. That keeps *Skuiz* unencumbered, but it does not remove the
//! obligation on anyone **shipping** a VST3 binary — Steinberg licenses the
//! VST3 format itself under either GPLv3 or a separate (free of charge)
//! proprietary agreement, and a closed-source plugin needs the latter.
//! That is why this crate is excluded from the workspace's default members:
//! building it should be a deliberate choice, not something a `cargo build`
//! does on your behalf. CLAP carries no such obligation.
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

use skuiz_core::{MidiOut, ParamDef, Processor};
use std::ffi::{c_char, c_void, CStr, CString};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComRef, ComWrapper};

pub use vst3;

/// Whether this platform has a webview editor backend in `skuiz-ui`.
const EDITOR_SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// The VST3 platform type matching this platform's `ParentView` constructor.
fn native_platform_type() -> FIDString {
    #[cfg(target_os = "windows")]
    {
        kPlatformTypeHWND
    }
    #[cfg(not(target_os = "windows"))]
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
    processor: Mutex<P>,
    midi_out: Mutex<MidiOut>,
    /// Timed parameter points collected from the host for the current
    /// block: `(sample_offset, param_id, plain_value)`. Audio-thread only;
    /// pre-allocated so `process` never allocates. The block is split at
    /// these offsets so automation lands sample-accurately.
    param_events: Mutex<Vec<(u32, u32, f64)>>,
    bus: Mutex<Option<skuiz_ipc::Bus>>,
    sync: Arc<SyncState>,
    /// The host's handler, retained while it is set. Editor-driven changes
    /// go through this, or the host never learns the GUI moved a parameter.
    handler: Mutex<Option<ComPtr<IComponentHandler>>>,
}

#[derive(Default)]
struct SyncState {
    pending: Mutex<Vec<(u32, f64)>>,
}

impl<P: Processor> Shared<P> {
    /// Apply an editor-driven parameter change: update the processor, tell
    /// the host so it records automation, and share it with other instances.
    fn set_param_from_editor(&self, id: u32, value: f64) {
        let Some(def) = P::params().iter().find(|p| p.id == id) else {
            return;
        };
        self.processor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_param(id, value);
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
        if let Ok(bus) = self.bus.lock() {
            if let Some(bus) = bus.as_ref() {
                bus.send(skuiz_core::protocol::set_param(id, value).as_bytes());
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
                processor: Mutex::new(P::default()),
                midi_out: Mutex::new(MidiOut::with_capacity(MIDI_OUT_CAPACITY)),
                param_events: Mutex::new(Vec::with_capacity(PARAM_EVENT_CAPACITY)),
                bus: Mutex::new(None),
                sync: Arc::new(SyncState::default()),
                handler: Mutex::new(None),
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

    /// Collect the block's timed parameter points into `out`
    /// (pre-allocated, never grows past [`PARAM_EVENT_CAPACITY`]). VST3
    /// sends a queue of timed points per parameter; every point is kept,
    /// converted to a plain value, clamped into the block, and sorted by
    /// sample offset so the block can be split at event times.
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
                    return; // full: drop the excess, same philosophy as MidiOut
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

    /// Convert generated MIDI into VST3 events. Note on/off map to native
    /// note events; other messages need `kLegacyMIDICCOutEvent` handling
    /// and are dropped for now. `offset` is the segment's start within the
    /// block (the block is split at parameter-point times, so the DSP's
    /// frame numbers are segment-relative); `frames` is the whole block.
    // ponytail: notes only. Add CC/pitch-bend as legacy MIDI CC events when
    // an example needs them.
    unsafe fn emit_events(&self, out: *mut IEventList, midi: &MidiOut, frames: usize, offset: u32) {
        let Some(list) = ComRef::from_raw(out) else {
            return;
        };
        for &(frame, bytes) in midi.events() {
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
                .processor
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .deactivate();
        }
        kResultOk
    }

    /// The whole stream is read before the processor lock is taken: stream
    /// I/O is unbounded and the audio thread contends on the same lock.
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
        let mut p = self
            .shared
            .processor
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if p.load_state(&data) {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn get_state_inner(&self, state: *mut IBStream) -> tresult {
        let Some(stream) = ComRef::from_raw(state) else {
            return kInvalidArgument;
        };
        // Serialise under the lock, then release it before the stream writes
        // — same reasoning as set_state_inner.
        let data = self
            .shared
            .processor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .save_state();
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
        self.shared
            .processor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .activate(setup.sampleRate, setup.maxSamplesPerBlock as u32);
        kResultOk
    }

    unsafe fn process_inner(&self, data: *mut ProcessData) -> tresult {
        let Some(data) = data.as_ref() else {
            return kInvalidArgument;
        };
        let frames = data.numSamples.max(0) as usize;

        // The block's timed automation points, sorted by sample offset.
        let mut events = self
            .shared
            .param_events
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        events.clear();
        self.collect_param_changes(data.inputParameterChanges, frames, &mut events);

        // One processor lock for the whole block. Values that arrived over
        // IPC carry no timing, so they apply at block top; host automation
        // lands sample-accurately in the segment loop below.
        let mut proc_ = self
            .shared
            .processor
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        {
            // Drained in place so the Vec's allocation survives — dropping
            // it here would free on the audio thread.
            let mut pending = self
                .shared
                .sync
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for &(id, value) in pending.iter() {
                if Self::param_by_id(id).is_some() {
                    proc_.set_param(id, value);
                }
            }
            pending.clear();
        }

        // Output channel pointers. A host that connects no outputs still
        // gets the processor run with zero channels, so DSP that only emits
        // MIDI keeps running.
        let mut outs: [*mut f32; 2] = [std::ptr::null_mut(); 2];
        let mut n_ch = 0;
        if data.numOutputs >= 1 && !data.outputs.is_null() {
            let out_bus = &*data.outputs;
            let out_ptrs = out_bus.__field0.channelBuffers32;
            if !out_ptrs.is_null() {
                n_ch = (out_bus.numChannels.max(0) as usize).min(outs.len());
                let out_bufs = std::slice::from_raw_parts(out_ptrs, n_ch);

                // Copy input to output unless the host processes in place.
                let mut copied = 0;
                if data.numInputs >= 1 && !data.inputs.is_null() {
                    let in_bus = &*data.inputs;
                    let in_ptrs = in_bus.__field0.channelBuffers32;
                    if !in_ptrs.is_null() {
                        let n_in = (in_bus.numChannels.max(0) as usize).min(n_ch);
                        let ins = std::slice::from_raw_parts(in_ptrs, n_in);
                        for c in 0..n_in {
                            if !ins[c].is_null() && !out_bufs[c].is_null() && ins[c] != out_bufs[c]
                            {
                                std::ptr::copy_nonoverlapping(ins[c], out_bufs[c], frames);
                            }
                        }
                        copied = n_in;
                    }
                }

                // Channels no input was copied into — every channel when the
                // host connects none — must be silenced, not left holding
                // whatever the buffer already contained.
                for out in out_bufs.iter().take(n_ch).skip(copied) {
                    if !out.is_null() {
                        std::ptr::write_bytes(*out, 0, frames);
                    }
                }

                for (c, slot) in outs.iter_mut().enumerate().take(n_ch) {
                    if out_bufs[c].is_null() {
                        n_ch = c;
                        break;
                    }
                    *slot = out_bufs[c];
                }
            }
        }

        // Split the block at point times: render up to the next point,
        // apply it, continue. Segments re-slice the same output buffers.
        let mut midi = self
            .shared
            .midi_out
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut pos = 0usize;
        for &(time, id, value) in events.iter() {
            let t = (time as usize).min(frames);
            if t > pos {
                self.process_segment(&mut proc_, &outs, n_ch, pos, t, &mut midi, data);
                pos = t;
            }
            proc_.set_param(id, value);
        }
        if pos < frames {
            self.process_segment(&mut proc_, &outs, n_ch, pos, frames, &mut midi, data);
        }
        kResultOk
    }

    /// Render one segment of a block: `outs[segment]` per channel, in
    /// place. MIDI queued by the DSP carries segment-relative frame numbers,
    /// so it is emitted with `segment.start` added back.
    #[allow(clippy::too_many_arguments)] // one concept per argument; a struct would be ceremony
    unsafe fn process_segment(
        &self,
        proc_: &mut P,
        outs: &[*mut f32; 2],
        n_ch: usize,
        start: usize,
        end: usize,
        midi: &mut MidiOut,
        data: &ProcessData,
    ) {
        let mut chans: [&mut [f32]; 2] = [&mut [], &mut []];
        for (c, chan) in chans.iter_mut().enumerate().take(n_ch) {
            *chan = std::slice::from_raw_parts_mut(outs[c].add(start), end - start);
        }
        midi.clear();
        proc_.process(&mut chans[..n_ch], midi);
        self.emit_events(
            data.outputEvents,
            midi,
            data.numSamples.max(0) as usize,
            start as u32,
        );
    }

    fn get_param_normalized_inner(&self, id: u32) -> f64 {
        let Some(def) = Self::param_by_id(id) else {
            return 0.0;
        };
        let p = self
            .shared
            .processor
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        to_normalized(def, p.get_param(id))
    }

    fn set_param_normalized_inner(&self, id: u32, value: f64) -> tresult {
        let Some(def) = Self::param_by_id(id) else {
            return kInvalidArgument;
        };
        self.shared
            .processor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_param(id, from_normalized(def, value));
        kResultOk
    }
}

impl<P: Processor + Default> Class for Vst3Plugin<P> {
    type Interfaces = (IComponent, IAudioProcessor, IEditController);
}

impl<P: Processor + Default> IPluginBaseTrait for Vst3Plugin<P> {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        // Join the IPC bus, exactly as the CLAP adapter does, so instances
        // share state across formats and processes alike.
        let sync = std::sync::Arc::clone(&self.shared.sync);
        let bus = skuiz_ipc::Bus::join(P::info().id, move |frame| {
            let Ok(msg) = std::str::from_utf8(frame) else {
                return;
            };
            let mut it = msg.split_whitespace();
            if it.next() != Some("set_param") {
                return;
            }
            let (Some(id), Some(value)) = (
                it.next().and_then(|s| s.parse::<u32>().ok()),
                it.next().and_then(|s| s.parse::<f64>().ok()),
            ) else {
                return;
            };
            let mut pending = sync.pending.lock().unwrap_or_else(|e| e.into_inner());
            // Coalesce by param id: only the latest value matters, and this
            // bounds the queue to the parameter count even if the host
            // stops calling process while messages keep arriving.
            match pending.iter_mut().find(|(pid, _)| *pid == id) {
                Some(entry) => entry.1 = value,
                None => pending.push((id, value)),
            }
        });
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
            MediaTypes_::kAudio => 1,
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
        if index != 0 || bus.is_null() {
            return kInvalidArgument;
        }
        let is_input = dir as BusDirections == BusDirections_::kInput;
        let bus = &mut *bus;
        match media_type as MediaTypes {
            MediaTypes_::kAudio => {
                bus.mediaType = MediaTypes_::kAudio as MediaType;
                bus.direction = dir;
                bus.channelCount = 2;
                copy_wstring(if is_input { "Input" } else { "Output" }, &mut bus.name);
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
            MediaTypes_::kEvent if P::emits_midi() && !is_input => {
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
        _media_type: MediaType,
        _dir: BusDirection,
        _index: i32,
        _state: TBool,
    ) -> tresult {
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
        if num_ins != 1 || num_outs != 1 || inputs.is_null() || outputs.is_null() {
            return kResultFalse;
        }
        if *inputs != SpeakerArr::kStereo || *outputs != SpeakerArr::kStereo {
            return kResultFalse;
        }
        kResultTrue
    }

    unsafe fn getBusArrangement(
        &self,
        _dir: BusDirection,
        index: i32,
        arr: *mut SpeakerArrangement,
    ) -> tresult {
        if index != 0 || arr.is_null() {
            return kInvalidArgument;
        }
        *arr = SpeakerArr::kStereo;
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
        0
    }

    unsafe fn setupProcessing(&self, setup: *mut ProcessSetup) -> tresult {
        ffi_guard(|| self.setup_processing_inner(setup), kResultFalse)
    }

    unsafe fn setProcessing(&self, _state: TBool) -> tresult {
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
            editor: std::cell::RefCell::new(None),
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
    /// Main-thread only: VST3 calls the view on the UI thread.
    editor: std::cell::RefCell<Option<skuiz_ui::Editor>>,
}

impl<P: Processor + Default> Class for Vst3PlugView<P> {
    type Interfaces = (IPlugView,);
}

impl<P: Processor + Default> IPlugViewTrait for Vst3PlugView<P> {
    unsafe fn isPlatformTypeSupported(&self, type_: FIDString) -> tresult {
        // ponytail: X11 is still missing, matching skuiz-ui.
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
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let Some(html) = P::editor_html() else {
                return kResultFalse;
            };
            #[cfg(target_os = "macos")]
            let view = skuiz_ui::ParentView::from_ns_view(parent);
            #[cfg(target_os = "windows")]
            let view = skuiz_ui::ParentView::from_hwnd(parent);
            let Some(view) = view else {
                return kInvalidArgument;
            };
            let shared = Arc::clone(&self.shared);
            let editor = skuiz_ui::Editor::attach(&view, html, P::editor_size(), move |msg| {
                let Some((id, value)) = skuiz_core::protocol::parse_set_param(&msg) else {
                    return;
                };
                shared.set_param_from_editor(id, value);
            });
            match editor {
                Ok(editor) => {
                    // Seed the page with the values the host currently
                    // holds — snapshot first so the processor lock is not
                    // held across the eval calls.
                    for (id, value) in skuiz_core::snapshot_params::<P>(&self.shared.processor) {
                        let _ = editor.eval(&skuiz_core::protocol::on_param_js(id, value));
                    }
                    *self.editor.borrow_mut() = Some(editor);
                    kResultOk
                }
                Err(_) => kResultFalse,
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
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

            #[cfg(target_os = "macos")]
            #[no_mangle]
            extern "system" fn BundleEntry(_bundle: *mut ::std::ffi::c_void) -> bool {
                true
            }

            #[cfg(target_os = "macos")]
            #[no_mangle]
            extern "system" fn BundleExit() -> bool {
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
