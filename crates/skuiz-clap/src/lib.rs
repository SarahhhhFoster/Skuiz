//! skuiz-clap: CLAP format adapter.
//!
//! Implement [`skuiz_core::Processor`] (plus `Default`) and export it from a
//! `cdylib` crate with `skuiz_clap::export_clap!(MyProcessor);`.

pub use clap_sys;
pub use skuiz_core;

use clap_sys::events::{
    clap_event_header, clap_event_midi, clap_event_midi2, clap_event_param_value,
    clap_input_events, clap_output_events, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI,
    CLAP_EVENT_MIDI2, CLAP_EVENT_PARAM_VALUE,
};
use clap_sys::ext::audio_ports::{
    clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS,
    CLAP_PORT_STEREO,
};
#[cfg(target_os = "macos")]
use clap_sys::ext::gui::CLAP_WINDOW_API_COCOA;
#[cfg(target_os = "windows")]
use clap_sys::ext::gui::CLAP_WINDOW_API_WIN32;
#[cfg(target_os = "linux")]
use clap_sys::ext::gui::CLAP_WINDOW_API_X11;
use clap_sys::ext::gui::{clap_gui_resize_hints, clap_plugin_gui, clap_window, CLAP_EXT_GUI};
use clap_sys::ext::latency::{clap_host_latency, clap_plugin_latency, CLAP_EXT_LATENCY};
use clap_sys::ext::note_ports::{
    clap_note_port_info, clap_plugin_note_ports, CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_MIDI,
    CLAP_NOTE_DIALECT_MIDI2,
};
use clap_sys::ext::params::{
    clap_host_params, clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS,
    CLAP_PARAM_IS_AUTOMATABLE, CLAP_PARAM_IS_ENUM, CLAP_PARAM_IS_STEPPED, CLAP_PARAM_RESCAN_VALUES,
};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::host::clap_host;
use clap_sys::id::{clap_id, CLAP_INVALID_ID};
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::{clap_process, clap_process_status, CLAP_PROCESS_CONTINUE};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::string_sizes::{CLAP_NAME_SIZE, CLAP_PATH_SIZE};
use clap_sys::version::CLAP_VERSION;
use skuiz_core::diag::DiagCounters;
use skuiz_core::engine::{AudioToken, Engine};
use skuiz_core::{MidiOut, Processor};
use std::cell::UnsafeCell;
use std::ffi::{c_char, c_void, CStr, CString};
use std::marker::PhantomData;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// A `clap_plugin_descriptor` plus ownership of the C strings it points at.
pub struct ClapDescriptor {
    _strings: Box<[CString]>,
    _features: Box<[*const c_char]>,
    /// The descriptor to hand the host. Valid as long as `self` lives.
    pub raw: clap_plugin_descriptor,
}

// Safety: immutable after construction; all pointers in `raw` target heap
// allocations owned by `_strings`/`_features`, which never move or drop
// before `self` does.
unsafe impl Send for ClapDescriptor {}
unsafe impl Sync for ClapDescriptor {}

impl ClapDescriptor {
    /// Build the descriptor for `P` from its [`skuiz_core::PluginInfo`].
    ///
    /// # Panics
    /// If any `PluginInfo` string contains an interior NUL byte.
    pub fn new<P: Processor>() -> Self {
        let info = P::info();
        let strings: Box<[CString]> = [
            info.id,
            info.name,
            info.vendor,
            info.version,
            info.description,
            "",
        ]
        .into_iter()
        .map(|s| CString::new(s).expect("PluginInfo strings must not contain NUL"))
        .collect();
        // A MIDI-emitting plugin advertises itself as an instrument; an
        // audio-only one as an effect.
        let feature = if P::emits_midi() {
            clap_sys::plugin_features::CLAP_PLUGIN_FEATURE_INSTRUMENT
        } else {
            clap_sys::plugin_features::CLAP_PLUGIN_FEATURE_AUDIO_EFFECT
        };
        let features: Box<[*const c_char]> = Box::new([feature.as_ptr(), null()]);
        let empty = strings[5].as_ptr();
        let raw = clap_plugin_descriptor {
            clap_version: CLAP_VERSION,
            id: strings[0].as_ptr(),
            name: strings[1].as_ptr(),
            vendor: strings[2].as_ptr(),
            url: empty,
            manual_url: empty,
            support_url: empty,
            version: strings[3].as_ptr(),
            description: strings[4].as_ptr(),
            features: features.as_ptr(),
        };
        Self {
            _strings: strings,
            _features: features,
            raw,
        }
    }
}

#[repr(C)]
struct Instance<P: Processor> {
    raw: clap_plugin,
    host: *const clap_host,
    // Main-thread only per the CLAP gui extension's threading rules.
    editor: std::cell::RefCell<Option<skuiz_ui::Editor>>,
    /// The engine owns the processor and everything realtime; the access
    /// protocol (docs/concepts/invariants.md) replaces the old
    /// Mutex-around-everything design. Arc'd so the bus callback's Weak
    /// handle can never use-after-free.
    engine: Arc<Engine<P>>,
    /// Proof the engine is in the AUDIO state, stashed between
    /// `start_processing` and `stop_processing` (or created on the fly by
    /// `process` for hosts that skip the pair). Audio-thread only.
    audio_token: std::cell::RefCell<Option<AudioToken>>,
    /// Timed parameter events collected from the host's input list for the
    /// current block: `(frame_offset, param_id, value)`. Audio-thread only;
    /// pre-allocated so `process` never allocates. The block is split at
    /// these times so automation lands sample-accurately.
    param_events: UnsafeCell<Vec<(u32, u32, f64)>>,
    /// The instance bus. Main thread only (joined in init, left at
    /// destroy); the bus callback itself uses the engine handle.
    bus: Mutex<Option<skuiz_ipc::Bus>>,
    /// Last-writer-wins versions for shared parameters (invariant 9); bus
    /// and UI threads only.
    lww: Arc<skuiz_core::lww::Lww>,
    // Liveness flag handed to the webview's IPC closure, which captures a
    // raw pointer back to this instance. Cleared before the editor (or the
    // instance) drops, so a message already in flight can never dereference
    // freed memory. Main-thread only, like `editor`.
    editor_alive: std::cell::RefCell<Option<Arc<AtomicBool>>>,
}

/// Whether this platform has a webview editor backend in `skuiz-ui`.
const EDITOR_SUPPORTED: bool = cfg!(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux"
));

/// The CLAP window API matching this platform's `ParentView` constructor.
fn native_window_api() -> &'static CStr {
    #[cfg(target_os = "windows")]
    {
        CLAP_WINDOW_API_WIN32
    }
    #[cfg(target_os = "linux")]
    {
        CLAP_WINDOW_API_X11
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "linux")))]
    {
        CLAP_WINDOW_API_COCOA
    }
}

/// Events one block may emit before further MIDI is dropped.
const MIDI_OUT_CAPACITY: usize = 512;

/// Timed parameter events one block may carry before the excess is dropped.
/// Like [`MidiOut`], the buffer is pre-allocated and fixed: a host sending
/// more automation points than this in a single block is pathological.
const PARAM_EVENT_CAPACITY: usize = 256;

/// Most state a host stream may feed us before we call it hostile.
const MAX_STATE_BYTES: usize = 16 * 1024 * 1024;

unsafe fn inst<'a, P: Processor>(plugin: *const clap_plugin) -> &'a Instance<P> {
    &*((*plugin).plugin_data as *const Instance<P>)
}

fn write_cstr(dst: &mut [c_char], s: &str) {
    if dst.is_empty() {
        return;
    }
    let n = s.len().min(dst.len() - 1);
    for (d, b) in dst.iter_mut().zip(s.as_bytes()[..n].iter()) {
        *d = *b as c_char;
    }
    dst[n] = 0;
}

/// Run a vtable body, turning a panic into `fallback` rather than
/// unwinding into the host across the extern "C" boundary (which is UB).
#[doc(hidden)]
pub fn ffi_guard<T>(fallback: T, body: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or(fallback)
}

/// Walk a host input event list, invoking `f` for every well-formed
/// PARAM_VALUE event naming a declared parameter. Shared by the flush paths
/// and by process()'s untimed application.
unsafe fn for_each_param_event<P: Processor>(
    list: *const clap_input_events,
    mut f: impl FnMut(u32, f64),
) {
    let Some(l) = list.as_ref() else { return };
    let (Some(size), Some(get)) = (l.size, l.get) else {
        return;
    };
    for i in 0..size(list) {
        let hdr = get(list, i);
        if hdr.is_null() {
            continue;
        }
        let h = &*hdr;
        // The header must be large enough to back the cast; a short event
        // is the host's bug, not license to read past what it sent.
        if h.space_id == CLAP_CORE_EVENT_SPACE_ID
            && h.type_ == CLAP_EVENT_PARAM_VALUE
            && h.size as usize >= std::mem::size_of::<clap_event_param_value>()
        {
            let ev = &*(hdr as *const clap_event_param_value);
            if P::params().iter().any(|p| p.id == ev.param_id) {
                f(ev.param_id, ev.value);
            }
        }
    }
}

unsafe fn apply_param_events<P: Processor>(
    proc_: &mut P,
    list: *const clap_input_events,
    mirror: &skuiz_core::rt::ParamMirror,
) {
    for_each_param_event::<P>(list, |id, value| {
        proc_.set_param(id, value);
        // Publish the readback, not the request: processors may round or
        // clamp, and the mirror must hold the value actually in force.
        mirror.publish(id, proc_.get_param(id));
    });
}

/// Collect the block's timed parameter events into `out` (pre-allocated,
/// never grows past [`PARAM_EVENT_CAPACITY`]), sorted by frame offset so
/// the block can be split at event times. Times are clamped into the
/// block; events for unknown param ids are skipped.
unsafe fn collect_param_events<P: Processor>(
    list: *const clap_input_events,
    frames: u32,
    out: &mut Vec<(u32, u32, f64)>,
    diag: &DiagCounters,
) {
    let Some(l) = list.as_ref() else { return };
    let (Some(size), Some(get)) = (l.size, l.get) else {
        return;
    };
    for i in 0..size(list) {
        let hdr = get(list, i);
        if hdr.is_null() {
            continue;
        }
        let h = &*hdr;
        if h.space_id == CLAP_CORE_EVENT_SPACE_ID
            && h.type_ == CLAP_EVENT_PARAM_VALUE
            && h.size as usize >= std::mem::size_of::<clap_event_param_value>()
        {
            let ev = &*(hdr as *const clap_event_param_value);
            if out.len() >= out.capacity() {
                // Overflow policy (invariant 8): drop the excess, count it.
                DiagCounters::bump(&diag.param_events_dropped);
                continue;
            }
            if P::params().iter().any(|p| p.id == ev.param_id) {
                out.push((ev.header.time.min(frames), ev.param_id, ev.value));
            }
        }
    }
    // Hosts should deliver events time-ordered; don't rely on it.
    out.sort_unstable_by_key(|e| e.0);
}

/// Push generated MIDI into the host's output queue. MIDI 1.0 messages go
/// out as native MIDI events; anything wider (MIDI 2.0) goes as a UMP
/// `clap_event_midi2`. `offset` is the segment's start within the block
/// (the block is split at parameter-event times, so the DSP's frame numbers
/// are segment-relative); `frames` is the whole block. Events past the end
/// of the block are clamped into it rather than dropped, since a host
/// rejects out-of-range timestamps.
unsafe fn emit_midi(midi: &MidiOut, out: *const clap_output_events, frames: usize, offset: u32) {
    let Some(list) = out.as_ref() else { return };
    let Some(try_push) = list.try_push else {
        return;
    };
    let last_frame = frames.saturating_sub(1) as u32;
    let header = |type_, size, time| clap_event_header {
        size,
        time,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_,
        flags: 0,
    };
    for &(frame, event) in midi.events() {
        let time = (frame + offset).min(last_frame);
        if let Some(data) = event.midi1_bytes() {
            let ev = clap_event_midi {
                header: header(
                    CLAP_EVENT_MIDI,
                    std::mem::size_of::<clap_event_midi>() as u32,
                    time,
                ),
                port_index: 0,
                data,
            };
            try_push(out, &ev.header);
        } else {
            let mut data = [0; 4];
            data[..event.words().len()].copy_from_slice(event.words());
            let ev = clap_event_midi2 {
                header: header(
                    CLAP_EVENT_MIDI2,
                    std::mem::size_of::<clap_event_midi2>() as u32,
                    time,
                ),
                port_index: 0,
                data,
            };
            try_push(out, &ev.header);
        }
    }
}

/// Allocate a plugin instance for `P`. Called by [`export_clap!`].
///
/// # Safety
/// `desc` must outlive the returned plugin (in practice: point into a static);
/// `host` is the host pointer from `create_plugin` (may be null in tests).
pub unsafe fn instantiate<P: Processor + Default>(
    desc: *const clap_plugin_descriptor,
    host: *const clap_host,
) -> *const clap_plugin {
    let boxed = Box::new(Instance::<P> {
        host,
        editor: std::cell::RefCell::new(None),
        editor_alive: std::cell::RefCell::new(None),
        engine: Engine::new(MIDI_OUT_CAPACITY),
        audio_token: std::cell::RefCell::new(None),
        param_events: UnsafeCell::new(Vec::with_capacity(PARAM_EVENT_CAPACITY)),
        bus: Mutex::new(None),
        lww: Arc::new(skuiz_core::lww::Lww::new()),
        raw: clap_plugin {
            desc,
            plugin_data: null_mut(),
            init: Some(Vt::<P>::init),
            destroy: Some(Vt::<P>::destroy),
            activate: Some(Vt::<P>::activate),
            deactivate: Some(Vt::<P>::deactivate),
            start_processing: Some(Vt::<P>::start_processing),
            stop_processing: Some(Vt::<P>::stop_processing),
            reset: Some(Vt::<P>::reset),
            process: Some(Vt::<P>::process),
            get_extension: Some(Vt::<P>::get_extension),
            on_main_thread: Some(Vt::<P>::on_main_thread),
        },
    });
    let ptr = Box::into_raw(boxed);
    (*ptr).raw.plugin_data = ptr as *mut c_void;
    &(*ptr).raw
}

#[allow(dead_code)]
struct Vt<P>(PhantomData<P>);

/// The block's output channels as raw pointers, grouped so
/// `Vt::process_segment` stays readable (and under the arg-count lint).
struct BlockOut {
    outs: [*mut f32; 2],
    n_ch: usize,
}

impl<P: Processor> Vt<P> {
    // --- plugin lifecycle -------------------------------------------------

    unsafe extern "C" fn init(plugin: *const clap_plugin) -> bool {
        ffi_guard(false, || {
            use skuiz_core::protocol as proto;
            let inst = inst::<P>(plugin);
            // Join the per-plugin IPC bus. The callback holds only an engine
            // handle (Weak), so a frame arriving after destroy goes nowhere.
            let handle = inst.engine.handle();
            let lww = Arc::clone(&inst.lww);
            let lww_cb = Arc::clone(&lww);
            // The callback answers sync_requests and link-ups, but the sender
            // only exists after join returns — hence the slot, filled below.
            let sender_slot: Arc<Mutex<Option<skuiz_ipc::BusSender>>> = Arc::new(Mutex::new(None));
            let cb_sender = Arc::clone(&sender_slot);
            let bus = skuiz_ipc::Bus::join(P::info().id, move |frame| {
                if frame == skuiz_ipc::LINK_UP_FRAME {
                    // The cross-process link is (back) up: ask everyone for
                    // shared state so frames dropped while it was down heal
                    // (invariant 9).
                    if let Some(sender) =
                        cb_sender.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
                    {
                        sender.send(proto::sync_request(lww_cb.origin()).as_bytes());
                    }
                    return;
                }
                let Ok(msg) = std::str::from_utf8(frame) else {
                    return;
                };
                if let Some((id, value, version)) = proto::parse_set_param_versioned(msg) {
                    // Local parameters never sync: frames naming them are
                    // ignored rather than applied (invariant 10). Stale
                    // versions lose (invariant 9). The version is recorded
                    // only if the change entered the engine, so a frame
                    // dropped by a full command queue can still win when
                    // re-delivered.
                    if !skuiz_core::syncs_over_bus::<P>(id) {
                        return;
                    }
                    lww_cb.accept_with(id, version, || handle.set_param(id, value));
                    return;
                }
                if proto::parse_sync_request(msg).is_some() {
                    // A late joiner asked for shared state; answer with the
                    // parameters we hold a *fresh* version for — ones edited
                    // over the bus and not rewritten by a project load
                    // since. Never-edited and post-load parameters are
                    // omitted: their value is host automation or project
                    // state, which is per-instance (invariant 10). Every
                    // instance answers — LWW makes duplicates safe.
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
                    if let Some(sender) =
                        cb_sender.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
                    {
                        sender.send(proto::sync_state(&entries).as_bytes());
                    }
                    return;
                }
                if let Some(entries) = proto::parse_sync_state(msg) {
                    for (id, value, seq, origin) in entries {
                        if skuiz_core::syncs_over_bus::<P>(id) {
                            lww_cb.accept_with(id, Some((seq, origin)), || {
                                handle.set_param(id, value)
                            });
                        }
                    }
                }
            });
            *sender_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(bus.sender());
            // Late joiner: ask the bus for current shared state. The initial
            // answer set is the convergence floor; LINK_UP covers reconnects.
            bus.send(proto::sync_request(lww.origin()).as_bytes());
            *inst.bus.lock().unwrap_or_else(|e| e.into_inner()) = Some(bus);
            true
        })
    }

    unsafe extern "C" fn destroy(plugin: *const clap_plugin) {
        ffi_guard((), || {
            if !plugin.is_null() {
                let boxed = Box::from_raw((*plugin).plugin_data as *mut Instance<P>);
                // In case the host never called gui_destroy: a webview
                // message arriving after this must not dereference us.
                if let Some(alive) = boxed.editor_alive.borrow().as_ref() {
                    alive.store(false, Ordering::Release);
                }
                drop(boxed);
            }
        })
    }

    unsafe extern "C" fn activate(
        plugin: *const clap_plugin,
        sample_rate: f64,
        _min_frames: u32,
        max_frames: u32,
    ) -> bool {
        ffi_guard(false, || {
            let engine = &inst::<P>(plugin).engine;
            // activate is main-thread with the transport stopped, so direct
            // access must succeed; a host calling it while processing is
            // breaking the spec, and we say no rather than race.
            let Some(latency) = engine.with_main(|core| {
                core.processor.activate(sample_rate, max_frames);
                core.processor.latency()
            }) else {
                return false;
            };
            engine.set_latency(latency);
            true
        })
    }

    unsafe extern "C" fn deactivate(plugin: *const clap_plugin) {
        ffi_guard((), || {
            let engine = &inst::<P>(plugin).engine;
            engine.with_main(|core| core.processor.deactivate());
        })
    }

    unsafe extern "C" fn start_processing(plugin: *const clap_plugin) -> bool {
        ffi_guard(false, || {
            let inst = inst::<P>(plugin);
            inst.audio_token.replace(Some(inst.engine.begin_audio()));
            true
        })
    }

    unsafe extern "C" fn stop_processing(plugin: *const clap_plugin) {
        ffi_guard((), || {
            let inst = inst::<P>(plugin);
            if let Some(token) = inst.audio_token.take() {
                inst.engine.end_audio(token);
            }
        })
    }

    /// `clap_plugin.reset`: main thread, transport running or stopped.
    /// `Engine::reset` covers both — direct when stopped, queued for the top
    /// of the next block when running, so DSP state clears between blocks.
    unsafe extern "C" fn reset(plugin: *const clap_plugin) {
        ffi_guard((), || {
            inst::<P>(plugin).engine.reset();
        })
    }

    /// Requested via `clap_host::request_callback` after remote (IPC) param
    /// changes landed: tell the host, refresh the editor. Also where a
    /// runtime latency change is announced (`clap_host_latency.changed`).
    unsafe extern "C" fn on_main_thread(plugin: *const clap_plugin) {
        ffi_guard((), || {
            let inst = inst::<P>(plugin);
            Self::host_rescan_params(inst.host);
            Self::sync_editor(inst);
            if inst.engine.take_latency_changed() {
                Self::host_latency_changed(inst.host);
            }
        })
    }

    /// Tell the host the plugin's latency changed, if it listens for that.
    unsafe fn host_latency_changed(host: *const clap_host) {
        let Some(h) = host.as_ref() else { return };
        let Some(get_ext) = h.get_extension else {
            return;
        };
        let ext = get_ext(host, CLAP_EXT_LATENCY.as_ptr());
        if ext.is_null() {
            return;
        }
        let host_latency = &*(ext as *const clap_host_latency);
        if let Some(changed) = host_latency.changed {
            changed(host);
        }
    }

    unsafe fn sync_editor(inst: &Instance<P>) {
        // Read the mirror, eval after: the mirror is wait-free, and
        // evaluate_script is an FFI call of unbounded cost that must never
        // hold anything the audio thread wants.
        let values = inst.engine.mirror().snapshot();
        if let Some(editor) = inst.editor.borrow().as_ref() {
            for (id, value) in values {
                let _ = editor.eval(&skuiz_core::protocol::on_param_js(id, value));
            }
        }
    }

    unsafe extern "C" fn process(
        plugin: *const clap_plugin,
        process: *const clap_process,
    ) -> clap_process_status {
        ffi_guard(CLAP_PROCESS_CONTINUE, || {
            let inst = inst::<P>(plugin);
            let p = &*process;
            let engine = &*inst.engine;

            // No locks anywhere below: the audio thread owns the processor
            // (invariant 1-2). Queued remote/editor/state changes carry no
            // timing, so they apply at block top; host automation lands
            // sample-accurately in the segment loop.
            //
            // Hosts should call start_processing first; tolerate the ones
            // (and the tests) that don't, rather than render silence.
            if !engine.is_processing() {
                inst.audio_token.replace(Some(engine.begin_audio()));
            }
            let core = {
                let token = inst.audio_token.borrow();
                engine.audio_core(token.as_ref().expect("AUDIO implies a token"))
            };
            let report = engine.drain_commands(core);
            let proc_ = &mut core.processor;

            // The block's timed automation events, sorted by frame offset.
            let events = &mut *inst.param_events.get();
            events.clear();
            collect_param_events::<P>(p.in_events, p.frames_count, events, engine.diag());
            // Assemble the output channel pointers. A plugin with no audio
            // output (or a host that gave us none) still gets processed
            // with zero channels, so DSP that only emits MIDI keeps running.
            let frames = p.frames_count as usize;
            let mut block = BlockOut {
                outs: [null_mut(), null_mut()],
                n_ch: 0,
            };
            if p.audio_outputs_count > 0 && !p.audio_outputs.is_null() {
                let out = &*p.audio_outputs;
                if !out.data32.is_null() {
                    block.n_ch = (out.channel_count as usize).min(block.outs.len());

                    // Copy input into output unless the host processes in
                    // place, then let the processor work in place on the
                    // outputs.
                    let mut copied = 0;
                    if p.audio_inputs_count > 0 && !p.audio_inputs.is_null() {
                        let inp = &*p.audio_inputs;
                        if !inp.data32.is_null() {
                            copied = block.n_ch.min(inp.channel_count as usize);
                            for c in 0..copied {
                                let src = *inp.data32.add(c);
                                let dst = *out.data32.add(c);
                                if !src.is_null() && !dst.is_null() && src != dst {
                                    std::ptr::copy_nonoverlapping(src, dst, frames);
                                }
                            }
                        }
                    }

                    // Zero the channels no input was copied into (host gave
                    // fewer inputs than outputs, or none): left alone they'd
                    // feed stale data from an earlier block to the DSP.
                    for c in copied..block.n_ch {
                        let dst = *out.data32.add(c);
                        if !dst.is_null() {
                            std::ptr::write_bytes(dst, 0, frames);
                        }
                    }

                    for (c, slot) in block.outs.iter_mut().enumerate().take(block.n_ch) {
                        let ptr = *out.data32.add(c);
                        if ptr.is_null() {
                            block.n_ch = c;
                            break;
                        }
                        *slot = ptr;
                    }
                }
            }

            // Split the block at event times: render up to the next event,
            // apply it, continue. Segments re-slice the same output buffers.
            let midi = &mut core.midi_out;
            let mut pos = 0usize;
            for &(time, id, value) in events.iter() {
                let t = (time as usize).min(frames);
                if t > pos {
                    Self::process_segment(proc_, &block, pos, t, midi, p, engine);
                    pos = t;
                }
                proc_.set_param(id, value);
                // Readback publish: the mirror must hold the value the
                // processor actually kept (it may round or clamp).
                engine.mirror().publish(id, proc_.get_param(id));
            }
            if pos < frames {
                Self::process_segment(proc_, &block, pos, frames, midi, p, engine);
            }
            if report.notify_main {
                // Bounce to the main thread for host rescan + editor refresh.
                if let Some(h) = inst.host.as_ref() {
                    if let Some(request_callback) = h.request_callback {
                        request_callback(inst.host);
                    }
                }
            }
            CLAP_PROCESS_CONTINUE
        })
    }

    /// Render one segment of a block: `outs[start..end]` per channel, in
    /// place. MIDI queued by the DSP carries segment-relative frame numbers,
    /// so it is emitted with `start` added back.
    unsafe fn process_segment(
        proc_: &mut P,
        block: &BlockOut,
        start: usize,
        end: usize,
        midi: &mut MidiOut,
        p: &clap_process,
        engine: &Engine<P>,
    ) {
        let mut chans: [&mut [f32]; 2] = [&mut [], &mut []];
        for (c, chan) in chans.iter_mut().enumerate().take(block.n_ch) {
            *chan = std::slice::from_raw_parts_mut(block.outs[c].add(start), end - start);
        }
        midi.clear();
        proc_.process(&mut chans[..block.n_ch], midi);
        // MIDI pushed past capacity is a counted drop (invariant 8).
        if midi.dropped() > 0 {
            engine
                .diag()
                .midi_events_dropped
                .fetch_add(midi.dropped() as u64, Ordering::Relaxed);
        }
        emit_midi(midi, p.out_events, p.frames_count as usize, start as u32);
    }

    unsafe extern "C" fn get_extension(
        _plugin: *const clap_plugin,
        id: *const c_char,
    ) -> *const c_void {
        ffi_guard(null(), || {
            if id.is_null() {
                return null();
            }
            let id = CStr::from_ptr(id);
            if id == CLAP_EXT_AUDIO_PORTS {
                &Self::AUDIO_PORTS as *const clap_plugin_audio_ports as *const c_void
            } else if id == CLAP_EXT_PARAMS {
                &Self::PARAMS as *const clap_plugin_params as *const c_void
            } else if id == CLAP_EXT_STATE {
                &Self::STATE as *const clap_plugin_state as *const c_void
            } else if id == CLAP_EXT_LATENCY {
                &Self::LATENCY as *const clap_plugin_latency as *const c_void
            } else if id == CLAP_EXT_NOTE_PORTS && P::emits_midi() {
                &Self::NOTE_PORTS as *const clap_plugin_note_ports as *const c_void
            } else if EDITOR_SUPPORTED && id == CLAP_EXT_GUI && P::editor_html().is_some() {
                &Self::GUI as *const clap_plugin_gui as *const c_void
            } else {
                null()
            }
        })
    }

    // --- audio-ports extension (fixed: one main stereo in, one out) -------

    const AUDIO_PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports {
        count: Some(Self::ap_count),
        get: Some(Self::ap_get),
    };

    unsafe extern "C" fn ap_count(_plugin: *const clap_plugin, _is_input: bool) -> u32 {
        ffi_guard(0, || 1)
    }

    unsafe extern "C" fn ap_get(
        _plugin: *const clap_plugin,
        index: u32,
        _is_input: bool,
        info: *mut clap_audio_port_info,
    ) -> bool {
        ffi_guard(false, || {
            if index != 0 || info.is_null() {
                return false;
            }
            let out = &mut *info;
            *out = clap_audio_port_info {
                id: 0,
                name: [0; CLAP_NAME_SIZE],
                flags: CLAP_AUDIO_PORT_IS_MAIN,
                channel_count: 2,
                port_type: CLAP_PORT_STEREO.as_ptr(),
                in_place_pair: CLAP_INVALID_ID,
            };
            write_cstr(&mut out.name, "main");
            true
        })
    }

    // --- latency extension -------------------------------------------------

    const LATENCY: clap_plugin_latency = clap_plugin_latency {
        get: Some(Self::latency_get),
    };

    unsafe extern "C" fn latency_get(plugin: *const clap_plugin) -> u32 {
        ffi_guard(0, || inst::<P>(plugin).engine.latency())
    }

    // --- note-ports extension (one output: MIDI 1.0 + MIDI 2.0 UMP) ------

    const NOTE_PORTS: clap_plugin_note_ports = clap_plugin_note_ports {
        count: Some(Self::np_count),
        get: Some(Self::np_get),
    };

    unsafe extern "C" fn np_count(_plugin: *const clap_plugin, is_input: bool) -> u32 {
        ffi_guard(0, || if is_input { 0 } else { 1 })
    }

    unsafe extern "C" fn np_get(
        _plugin: *const clap_plugin,
        index: u32,
        is_input: bool,
        info: *mut clap_note_port_info,
    ) -> bool {
        ffi_guard(false, || {
            if is_input || index != 0 || info.is_null() {
                return false;
            }
            let out = &mut *info;
            *out = clap_note_port_info {
                id: 0,
                supported_dialects: CLAP_NOTE_DIALECT_MIDI | CLAP_NOTE_DIALECT_MIDI2,
                preferred_dialect: CLAP_NOTE_DIALECT_MIDI,
                name: [0; CLAP_NAME_SIZE],
            };
            write_cstr(&mut out.name, "MIDI Out");
            true
        })
    }

    // --- params extension -------------------------------------------------

    const PARAMS: clap_plugin_params = clap_plugin_params {
        count: Some(Self::params_count),
        get_info: Some(Self::params_get_info),
        get_value: Some(Self::params_get_value),
        value_to_text: Some(Self::params_value_to_text),
        text_to_value: Some(Self::params_text_to_value),
        flush: Some(Self::params_flush),
    };

    unsafe extern "C" fn params_count(_plugin: *const clap_plugin) -> u32 {
        ffi_guard(0, || P::params().len() as u32)
    }

    unsafe extern "C" fn params_get_info(
        _plugin: *const clap_plugin,
        index: u32,
        info: *mut clap_param_info,
    ) -> bool {
        ffi_guard(false, || {
            let Some(def) = P::params().get(index as usize) else {
                return false;
            };
            if info.is_null() {
                return false;
            }
            // A param id of u32::MAX would alias CLAP_INVALID_ID; refuse to
            // publish it rather than hand the host a broken id.
            if def.id == CLAP_INVALID_ID {
                debug_assert!(false, "param id must not equal CLAP_INVALID_ID");
                return false;
            }
            let discrete = !def.choices.is_empty();
            let out = &mut *info;
            *out = clap_param_info {
                id: def.id,
                flags: CLAP_PARAM_IS_AUTOMATABLE
                    | if discrete {
                        CLAP_PARAM_IS_STEPPED | CLAP_PARAM_IS_ENUM
                    } else {
                        0
                    },
                cookie: null_mut(),
                name: [0; CLAP_NAME_SIZE],
                module: [0; CLAP_PATH_SIZE],
                min_value: def.low(),
                max_value: def.high(),
                default_value: def.default,
            };
            write_cstr(&mut out.name, def.name);
            true
        })
    }

    unsafe extern "C" fn params_get_value(
        plugin: *const clap_plugin,
        param_id: clap_id,
        out_value: *mut f64,
    ) -> bool {
        ffi_guard(false, || {
            // Reads come from the mirror (invariant 6): wait-free, no
            // processor access.
            if out_value.is_null() {
                return false;
            }
            match inst::<P>(plugin).engine.mirror().get(param_id) {
                Some(v) => {
                    *out_value = v;
                    true
                }
                None => false,
            }
        })
    }

    unsafe extern "C" fn params_value_to_text(
        _plugin: *const clap_plugin,
        param_id: clap_id,
        value: f64,
        out_buffer: *mut c_char,
        out_buffer_capacity: u32,
    ) -> bool {
        ffi_guard(false, || {
            if out_buffer.is_null() || out_buffer_capacity == 0 {
                return false;
            }
            let Some(def) = P::params().iter().find(|p| p.id == param_id) else {
                return false;
            };
            // Out-of-range choice values fall through to the number, which
            // surfaces the anomaly instead of showing a plausible wrong label.
            let s = match def.label(value) {
                Some(label) => label.to_string(),
                // Shortest representation that parses back to the same f64.
                None => format!("{value}"),
            };
            let dst = std::slice::from_raw_parts_mut(out_buffer, out_buffer_capacity as usize);
            write_cstr(dst, &s);
            true
        })
    }

    unsafe extern "C" fn params_text_to_value(
        _plugin: *const clap_plugin,
        param_id: clap_id,
        text: *const c_char,
        out_value: *mut f64,
    ) -> bool {
        ffi_guard(false, || {
            if text.is_null() || out_value.is_null() {
                return false;
            }
            let Ok(text) = CStr::from_ptr(text).to_str() else {
                return false;
            };
            let text = text.trim();
            let Some(def) = P::params().iter().find(|p| p.id == param_id) else {
                return false;
            };
            // A choice parameter accepts its own label back; anything else is
            // parsed as a plain number.
            if let Some(idx) = def.choices.iter().position(|c| *c == text) {
                *out_value = idx as f64;
                return true;
            }
            match text.parse::<f64>() {
                Ok(v) => {
                    *out_value = v;
                    true
                }
                Err(_) => false,
            }
        })
    }

    /// `clap_plugin_params.flush`: the spec threads this as [audio-thread
    /// when processing, main-thread otherwise], which is exactly the
    /// dispatch below. Flush events are untimed — "apply these now" — so
    /// they apply in list order, last write wins; sample-accurate timed
    /// delivery stays `process()`'s job. Both paths publish the processor's
    /// readback to the mirror, same as the sample-accurate path.
    unsafe extern "C" fn params_flush(
        plugin: *const clap_plugin,
        in_events: *const clap_input_events,
        _out_events: *const clap_sys::events::clap_output_events,
    ) {
        ffi_guard((), || {
            let inst = inst::<P>(plugin);
            let engine = &inst.engine;
            if engine.is_processing() {
                // Audio-thread flush: same rules as process().
                let token = inst.audio_token.borrow();
                let core = engine.audio_core(token.as_ref().expect("AUDIO implies a token"));
                apply_param_events::<P>(&mut core.processor, in_events, engine.mirror());
            } else if engine
                .with_main(|core| {
                    apply_param_events::<P>(&mut core.processor, in_events, engine.mirror())
                })
                .is_none()
            {
                // The stopped main state is contended (a state op in
                // flight): route each event through the engine instead of
                // dropping it silently — set_param applies synchronously on
                // a stopped engine, or counts the drop (invariant 8).
                for_each_param_event::<P>(in_events, |id, value| {
                    engine.set_param(id, value);
                });
            }
        })
    }

    // --- gui extension (webview editor) -----------------------------------

    const GUI: clap_plugin_gui = clap_plugin_gui {
        is_api_supported: Some(Self::gui_is_api_supported),
        get_preferred_api: Some(Self::gui_get_preferred_api),
        create: Some(Self::gui_create),
        destroy: Some(Self::gui_destroy),
        set_scale: Some(Self::gui_set_scale),
        get_size: Some(Self::gui_get_size),
        can_resize: Some(Self::gui_can_resize),
        get_resize_hints: Some(Self::gui_get_resize_hints),
        adjust_size: Some(Self::gui_adjust_size),
        set_size: Some(Self::gui_set_size),
        set_parent: Some(Self::gui_set_parent),
        set_transient: Some(Self::gui_set_transient),
        suggest_title: Some(Self::gui_suggest_title),
        show: Some(Self::gui_show),
        hide: Some(Self::gui_hide),
    };

    unsafe extern "C" fn gui_is_api_supported(
        _plugin: *const clap_plugin,
        api: *const c_char,
        is_floating: bool,
    ) -> bool {
        ffi_guard(false, || {
            !is_floating && !api.is_null() && CStr::from_ptr(api) == native_window_api()
        })
    }

    unsafe extern "C" fn gui_get_preferred_api(
        _plugin: *const clap_plugin,
        api: *mut *const c_char,
        is_floating: *mut bool,
    ) -> bool {
        ffi_guard(false, || {
            if api.is_null() || is_floating.is_null() {
                return false;
            }
            *api = native_window_api().as_ptr();
            *is_floating = false;
            true
        })
    }

    unsafe extern "C" fn gui_create(
        plugin: *const clap_plugin,
        api: *const c_char,
        is_floating: bool,
    ) -> bool {
        // The editor itself is built in set_parent, once we have the view.
        Self::gui_is_api_supported(plugin, api, is_floating)
    }

    unsafe extern "C" fn gui_destroy(plugin: *const clap_plugin) {
        ffi_guard((), || {
            let inst = inst::<P>(plugin);
            // Sever the webview's route back into this instance before
            // dropping the editor; a message already in flight must not
            // dereference a half-destroyed instance.
            if let Some(alive) = inst.editor_alive.borrow().as_ref() {
                alive.store(false, Ordering::Release);
            }
            inst.editor.replace(None);
        })
    }

    unsafe extern "C" fn gui_set_scale(_plugin: *const clap_plugin, _scale: f64) -> bool {
        ffi_guard(false, || true)
    }

    unsafe extern "C" fn gui_get_size(
        _plugin: *const clap_plugin,
        width: *mut u32,
        height: *mut u32,
    ) -> bool {
        ffi_guard(false, || {
            if width.is_null() || height.is_null() {
                return false;
            }
            let (w, h) = P::editor_size();
            *width = w;
            *height = h;
            true
        })
    }

    unsafe extern "C" fn gui_can_resize(_plugin: *const clap_plugin) -> bool {
        false
    }

    unsafe extern "C" fn gui_get_resize_hints(
        _plugin: *const clap_plugin,
        _hints: *mut clap_gui_resize_hints,
    ) -> bool {
        false
    }

    unsafe extern "C" fn gui_adjust_size(
        plugin: *const clap_plugin,
        width: *mut u32,
        height: *mut u32,
    ) -> bool {
        Self::gui_get_size(plugin, width, height)
    }

    unsafe extern "C" fn gui_set_size(
        _plugin: *const clap_plugin,
        width: u32,
        height: u32,
    ) -> bool {
        ffi_guard(false, || (width, height) == P::editor_size())
    }

    unsafe extern "C" fn gui_set_parent(
        plugin: *const clap_plugin,
        window: *const clap_window,
    ) -> bool {
        ffi_guard(false, || {
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            {
                let Some(html) = P::editor_html() else {
                    return false;
                };
                let Some(w) = window.as_ref() else {
                    return false;
                };
                if w.api.is_null() || CStr::from_ptr(w.api) != native_window_api() {
                    return false;
                }
                #[cfg(target_os = "macos")]
                let parent = skuiz_ui::ParentView::from_ns_view(w.specific.cocoa);
                #[cfg(target_os = "windows")]
                let parent = skuiz_ui::ParentView::from_hwnd(w.specific.win32);
                #[cfg(target_os = "linux")]
                let parent = skuiz_ui::ParentView::from_x11(w.specific.x11);
                let Some(parent) = parent else {
                    return false;
                };
                // The message closure outlives this call and fires from
                // webview threads; the alive flag is its only license to
                // dereference the raw plugin pointer it captures.
                let alive = Arc::new(AtomicBool::new(true));
                let addr = plugin as usize;
                let editor = {
                    let alive = Arc::clone(&alive);
                    skuiz_ui::Editor::attach(&parent, html, P::editor_size(), move |msg| {
                        if alive.load(Ordering::Acquire) {
                            Self::handle_ui_message(addr as *const clap_plugin, &msg);
                        }
                    })
                };
                let inst = inst::<P>(plugin);
                match editor {
                    Ok(editor) => {
                        inst.editor.replace(Some(editor));
                        inst.editor_alive.replace(Some(alive));
                        // Seed the page with current parameter values.
                        Self::sync_editor(inst);
                        true
                    }
                    Err(_) => false,
                }
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
            {
                let _ = (plugin, window);
                false
            }
        })
    }

    unsafe extern "C" fn gui_set_transient(
        _plugin: *const clap_plugin,
        _window: *const clap_window,
    ) -> bool {
        false
    }

    unsafe extern "C" fn gui_suggest_title(_plugin: *const clap_plugin, _title: *const c_char) {}

    unsafe extern "C" fn gui_show(_plugin: *const clap_plugin) -> bool {
        ffi_guard(false, || true)
    }

    unsafe extern "C" fn gui_hide(_plugin: *const clap_plugin) -> bool {
        ffi_guard(false, || true)
    }

    /// UI -> plugin protocol: `"set_param <id> <value>"`, plus the typed
    /// `"skuiz_diag"` query answered with a `skuizOnDiag` eval.
    // ponytail: GUI param changes reach the host via rescan(VALUES), which
    // syncs values but doesn't record automation gestures; switch to
    // request_flush + output events when automation recording matters.
    unsafe fn handle_ui_message(plugin: *const clap_plugin, msg: &str) {
        let inst = inst::<P>(plugin);
        if msg == skuiz_core::protocol::DIAG_QUERY {
            // Diagnostics for the page: counters are atomics, so this is
            // a plain read; the eval back is main-thread editor work.
            let js = skuiz_core::protocol::on_diag_js(inst.engine.diag());
            if let Some(editor) = inst.editor.borrow().as_ref() {
                let _ = editor.eval(&js);
            }
            return;
        }
        let Some((id, value)) = skuiz_core::protocol::parse_set_param(msg) else {
            return;
        };
        let Some(def) = P::params().iter().find(|p| p.id == id) else {
            return;
        };
        // Direct when stopped, queued for the next block when running;
        // never locks the processor (invariants 2, 5). For shared
        // parameters the apply happens inside `stamp_with`: only a change
        // that entered the engine claims a version and reaches the bus,
        // so a dropped command never splits the instance from its peers
        // (invariant 9).
        if def.shared {
            let stamped = inst.lww.stamp_with(id, || inst.engine.set_param(id, value));
            let Some((seq, origin)) = stamped else {
                return;
            };
            Self::host_rescan_params(inst.host);
            // Share the UI move with every other instance on the bus. The
            // versioned frame lets receivers discard stale echoes.
            if let Some(bus) = inst.bus.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                bus.send(
                    skuiz_core::protocol::set_param_versioned(id, value, seq, origin).as_bytes(),
                );
            }
        } else {
            // Local parameters stay in the instance (invariant 10).
            if inst.engine.set_param(id, value) {
                Self::host_rescan_params(inst.host);
            }
        }
    }

    // --- state extension --------------------------------------------------

    const STATE: clap_plugin_state = clap_plugin_state {
        save: Some(Self::state_save),
        load: Some(Self::state_load),
    };

    unsafe extern "C" fn state_save(
        plugin: *const clap_plugin,
        stream: *const clap_ostream,
    ) -> bool {
        ffi_guard(false, || {
            let Some(s) = stream.as_ref() else {
                return false;
            };
            let Some(write) = s.write else { return false };
            // Direct when stopped, a bounded round-trip through the audio
            // thread when running; None means the round-trip timed out.
            let Some(data) = inst::<P>(plugin).engine.save_state() else {
                return false;
            };
            let mut off = 0;
            while off < data.len() {
                let n = write(
                    stream,
                    data[off..].as_ptr() as *const c_void,
                    (data.len() - off) as u64,
                );
                if n <= 0 {
                    return false;
                }
                off += n as usize;
            }
            true
        })
    }

    unsafe extern "C" fn state_load(
        plugin: *const clap_plugin,
        stream: *const clap_istream,
    ) -> bool {
        ffi_guard(false, || {
            let Some(s) = stream.as_ref() else {
                return false;
            };
            let Some(read) = s.read else { return false };
            let mut data = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = read(stream, buf.as_mut_ptr() as *mut c_void, buf.len() as u64);
                if n < 0 {
                    return false;
                }
                if n == 0 {
                    break;
                }
                // A stream reporting more than it could have written is
                // corrupt; slicing `buf[..n]` anyway would read unwritten
                // memory.
                if n as usize > buf.len() {
                    return false;
                }
                data.extend_from_slice(&buf[..n as usize]);
                // ...and one that never signals EOF must not grow the
                // buffer without bound.
                if data.len() > MAX_STATE_BYTES {
                    return false;
                }
            }
            let inst = inst::<P>(plugin);
            if !inst.engine.load_state(data) {
                return false;
            }
            // Project state replaced the parameter values without versions:
            // stop advertising them to the bus until a shared edit lands
            // (invariant 10).
            inst.lww.on_state_load();
            // Loading changed parameter values; the host must be told to rescan.
            Self::host_rescan_params(inst.host);
            true
        })
    }

    unsafe fn host_rescan_params(host: *const clap_host) {
        let Some(h) = host.as_ref() else { return };
        let Some(get_ext) = h.get_extension else {
            return;
        };
        let ext = get_ext(host, CLAP_EXT_PARAMS.as_ptr());
        if ext.is_null() {
            return;
        }
        let host_params = &*(ext as *const clap_host_params);
        if let Some(rescan) = host_params.rescan {
            rescan(host, CLAP_PARAM_RESCAN_VALUES);
        }
    }
}

/// Export `$P` as this cdylib's CLAP entry point.
#[macro_export]
macro_rules! export_clap {
    ($P:ty) => {
        const _: () = {
            use $crate::clap_sys as cs;

            static DESCRIPTOR: ::std::sync::OnceLock<$crate::ClapDescriptor> =
                ::std::sync::OnceLock::new();

            fn descriptor() -> &'static cs::plugin::clap_plugin_descriptor {
                &DESCRIPTOR
                    .get_or_init($crate::ClapDescriptor::new::<$P>)
                    .raw
            }

            unsafe extern "C" fn get_plugin_count(
                _factory: *const cs::factory::plugin_factory::clap_plugin_factory,
            ) -> u32 {
                $crate::ffi_guard(0, || 1)
            }

            unsafe extern "C" fn get_plugin_descriptor(
                _factory: *const cs::factory::plugin_factory::clap_plugin_factory,
                index: u32,
            ) -> *const cs::plugin::clap_plugin_descriptor {
                $crate::ffi_guard(::std::ptr::null(), || {
                    if index == 0 {
                        descriptor()
                    } else {
                        ::std::ptr::null()
                    }
                })
            }

            unsafe extern "C" fn create_plugin(
                _factory: *const cs::factory::plugin_factory::clap_plugin_factory,
                host: *const cs::host::clap_host,
                plugin_id: *const ::std::ffi::c_char,
            ) -> *const cs::plugin::clap_plugin {
                $crate::ffi_guard(::std::ptr::null(), || {
                    if plugin_id.is_null() {
                        return ::std::ptr::null();
                    }
                    let want = <$P as $crate::skuiz_core::Processor>::info().id;
                    if ::std::ffi::CStr::from_ptr(plugin_id).to_str() != Ok(want) {
                        return ::std::ptr::null();
                    }
                    $crate::instantiate::<$P>(descriptor(), host)
                })
            }

            static FACTORY: cs::factory::plugin_factory::clap_plugin_factory =
                cs::factory::plugin_factory::clap_plugin_factory {
                    get_plugin_count: Some(get_plugin_count),
                    get_plugin_descriptor: Some(get_plugin_descriptor),
                    create_plugin: Some(create_plugin),
                };

            unsafe extern "C" fn entry_init(_plugin_path: *const ::std::ffi::c_char) -> bool {
                $crate::ffi_guard(false, || true)
            }

            unsafe extern "C" fn entry_deinit() {}

            unsafe extern "C" fn entry_get_factory(
                factory_id: *const ::std::ffi::c_char,
            ) -> *const ::std::ffi::c_void {
                $crate::ffi_guard(::std::ptr::null(), || {
                    if !factory_id.is_null()
                        && ::std::ffi::CStr::from_ptr(factory_id)
                            == cs::factory::plugin_factory::CLAP_PLUGIN_FACTORY_ID
                    {
                        &FACTORY as *const cs::factory::plugin_factory::clap_plugin_factory
                            as *const ::std::ffi::c_void
                    } else {
                        ::std::ptr::null()
                    }
                })
            }

            #[allow(non_upper_case_globals)]
            #[no_mangle]
            pub static clap_entry: cs::entry::clap_plugin_entry = cs::entry::clap_plugin_entry {
                clap_version: cs::version::CLAP_VERSION,
                init: Some(entry_init),
                deinit: Some(entry_deinit),
                get_factory: Some(entry_get_factory),
            };
        };
    };
}
