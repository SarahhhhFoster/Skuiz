//! skuiz-auv3: the Rust half of an Audio Unit v3 plugin.
//!
//! # What this is, and what it is not
//!
//! AUv3 is the one target Skuiz cannot reach from Rust alone. An Audio Unit
//! v3 is a macOS/iOS *app extension*: the host loads a bundle whose
//! principal class is an Objective-C `AUAudioUnit` subclass, and that bundle
//! must be built, signed and embedded in a containing app by Xcode. There is
//! no entry-point symbol we can export to make that happen.
//!
//! This crate therefore ships both halves of what can be built outside
//! Xcode: [`export_auv3!`] generates a flat C ABI, and `shim/` holds the
//! `AUAudioUnit` subclass that calls into it, compiled automatically on
//! Apple targets. `scaffold/` holds the Info.plist and entitlements.
//!
//! The shim is *executed*, not just compiled: an `AUAudioUnit` subclass can
//! be instantiated and rendered in-process without an extension bundle,
//! host, or code signing, so `skuiz_auv3_selftest` drives a real unit
//! through parameters, rendering, MIDI output and state, and the Rust test
//! suite runs it. What remains genuinely untestable here is the packaging:
//! the Xcode target, signing, and a host discovering the component.
//!
//! # The AUv3 process model, and what it means for IPC
//!
//! Apple confirmed (developer forums thread 65909) that **every instance of
//! the same audio unit inside one host loads into a single shared extension
//! process**, and that there is no way to force separate processes. Two
//! consequences that are easy to get backwards:
//!
//! - **Within one host, instances share an address space**, and the bus
//!   delivers between them by direct call — no socket, and therefore no App
//!   Group. Passing `NULL` for `app_group_dir` is correct for this case, and
//!   sync still works even if the sandbox blocks the socket path entirely.
//! - **Across hosts, instances really are separate processes.** Two apps
//!   hosting the same plugin get an extension process each, and only then
//!   does the sandbox block a shared socket path. That is what the **App
//!   Group** container is for: pass its path to `skuiz_auv3_init` and it is
//!   forwarded to [`skuiz_ipc::Bus::join_in`]. See
//!   `scaffold/Skuiz.entitlements`.
//!
//! A third consequence is worth knowing even though Skuiz cannot fix it: a
//! crash in one instance takes down the shared process, and with it every
//! other instance of that plugin. The host survives. Server promotion still
//! earns its place, because the ordinary case — the user deleting the
//! instance that happened to be serving — leaves the process alive.

use skuiz_core::bus::{AudioBusSpec, BusDirection};
use skuiz_core::engine::{AudioToken, Engine};
use skuiz_core::Processor;
use std::cell::RefCell;
use std::ffi::{c_char, CStr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Events one block may emit before further MIDI is dropped.
const MIDI_OUT_CAPACITY: usize = 512;

/// Run `f`, returning `fallback` if it panics. A panic unwinding across the
/// C ABI is UB and would take down the whole extension process (and every
/// sibling instance with it), so exported functions go through here.
#[doc(hidden)]
pub fn ffi_guard<R>(f: impl FnOnce() -> R, fallback: R) -> R {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(fallback)
}

/// Parameter description handed to the Objective-C side, which turns each
/// one into an `AUParameter`. Choice parameters arrive with
/// `choice_count > 0`; fetch their labels with `skuiz_auv3_choice_label`.
#[repr(C)]
pub struct SkuizParamInfo {
    /// Parameter id, used as the `AUParameterAddress`.
    pub id: u32,
    /// NUL-terminated, owned by Rust and valid for the process lifetime
    /// (parameter names are `&'static str`).
    pub name: *const c_char,
    /// Lowest legal value; 0 for a choice parameter.
    pub min: f64,
    /// Highest legal value; the last index for a choice parameter.
    pub max: f64,
    /// Initial value.
    pub default: f64,
    /// Number of choices, or 0 for a continuous parameter. When non-zero,
    /// fetch each label with `skuiz_auv3_choice_label`.
    pub choice_count: u32,
}

/// Static description of one declared audio bus, handed to the Objective-C
/// side, which turns each into an `AUAudioUnitBus`.
#[repr(C)]
pub struct SkuizAudioBusInfo {
    /// Channels in the declared layout.
    pub channel_count: u32,
    /// Non-zero when the host may leave this bus disconnected (never the
    /// main bus of a direction).
    pub optional: u8,
    /// NUL-terminated, owned by Rust and valid for the process lifetime (bus
    /// names are `&'static str`); null for an empty name.
    pub name: *const c_char,
}

/// One bus's channel pointers for a render call, mirroring the shim's
/// `SkuizAudioBusBuffers`. The shim passes a fixed array of
/// [`skuiz_core::bus::MAX_BUSES_PER_DIRECTION`] of these per direction, in
/// declaration order; slots past the declared count stay zeroed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SkuizAudioBusBuffers {
    /// Channel buffers, valid for `frames` samples; only the first
    /// `channel_count` entries are read.
    pub channels: [*mut f32; skuiz_core::bus::MAX_BUS_CHANNELS],
    /// Channels actually supplied for this bus; the Rust side clamps to the
    /// declared layout.
    pub channel_count: u32,
    /// Non-zero when the host connected/activated this bus for this block.
    pub active: u8,
}

impl SkuizAudioBusBuffers {
    /// An inactive bus with no channels.
    pub const fn empty() -> Self {
        Self {
            channels: [std::ptr::null_mut(); skuiz_core::bus::MAX_BUS_CHANNELS],
            channel_count: 0,
            active: 0,
        }
    }
}

impl Default for SkuizAudioBusBuffers {
    fn default() -> Self {
        Self::empty()
    }
}

/// The declared buses in one direction (`0` = input, anything else =
/// output), in declaration order.
#[doc(hidden)]
pub fn bus_spec<P: Processor>(direction: u8, index: u32) -> Option<&'static AudioBusSpec> {
    let dir = if direction == 0 {
        BusDirection::Input
    } else {
        BusDirection::Output
    };
    P::audio_buses()
        .iter()
        .filter(move |s| s.direction == dir)
        .nth(index as usize)
}

/// NUL-terminated bus name for the C side, or null for an empty name.
///
/// Bus names are `&'static str`, so the leaked `CString` is a one-time,
/// bounded cost keyed by the string's address.
pub fn bus_name_ptr(name: &'static str) -> *const c_char {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    if name.is_empty() {
        return std::ptr::null();
    }
    static NAMES: OnceLock<CStrCache<usize>> = OnceLock::new();
    let map = NAMES.get_or_init(|| Mutex::new(HashMap::new()));

    let key = name.as_ptr() as usize;
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(found) = map.get(&key) {
        return found.as_ptr();
    }
    let owned: &'static CStr = Box::leak(
        std::ffi::CString::new(name)
            .unwrap_or_default()
            .into_boxed_c_str(),
    );
    map.insert(key, owned);
    owned.as_ptr()
}

/// The live plugin instance behind the opaque pointer the shim holds.
pub struct AuInstance<P: Processor> {
    /// Owns the processor, MIDI scratch and the command queue. The render
    /// thread claims the audio state around each render call; between calls
    /// the engine is idle and main-thread entry points take the direct
    /// path. Neither side ever locks the processor.
    pub engine: Arc<Engine<P>>,
    /// Proof the engine is in the AUDIO state, stashed for the duration of
    /// a render call: claimed at the top of `skuiz_auv3_render`, handed back
    /// at the bottom. Render-thread only.
    pub audio_token: RefCell<Option<AudioToken>>,
    /// This instance's seat on the shared-state bus.
    pub bus: Mutex<Option<skuiz_ipc::Bus>>,
    /// Last-writer-wins versions for shared parameters (invariant 9); bus
    /// and UI threads only.
    pub lww: Arc<skuiz_core::lww::Lww>,
}

impl<P: Processor + Default> Default for AuInstance<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: Processor + Default> AuInstance<P> {
    /// Create an instance with every parameter at its default. Join the bus
    /// separately with [`AuInstance::join_bus`].
    pub fn new() -> Self {
        Self {
            engine: Engine::new(MIDI_OUT_CAPACITY),
            audio_token: RefCell::new(None),
            bus: Mutex::new(None),
            lww: Arc::new(skuiz_core::lww::Lww::new()),
        }
    }

    /// Join the IPC bus, placing the socket in `group_dir` when the host
    /// sandbox requires it.
    ///
    /// Bus callbacks run on a bus thread and must never touch plugin memory
    /// directly, so they push through an [`skuiz_core::engine::EngineHandle`]
    /// instead: the command lands in the bounded queue the render thread
    /// drains, and the handle's `Weak` means a frame in flight after
    /// instance destroy simply goes nowhere.
    pub fn join_bus(&self, group_dir: Option<PathBuf>) {
        use skuiz_core::protocol as proto;
        let handle = self.engine.handle();
        let lww = Arc::clone(&self.lww);
        let lww_cb = Arc::clone(&lww);
        // The callback answers sync_requests and link-ups, but the sender
        // only exists after join returns — hence the slot, filled below.
        let sender_slot: Arc<Mutex<Option<skuiz_ipc::BusSender>>> = Arc::new(Mutex::new(None));
        let cb_sender = Arc::clone(&sender_slot);
        let on_message = move |frame: &[u8]| {
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
        };
        let bus = match group_dir {
            Some(dir) => skuiz_ipc::Bus::join_in(&dir, P::info().id, on_message),
            None => skuiz_ipc::Bus::join(P::info().id, on_message),
        };
        *sender_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(bus.sender());
        // Late joiner: ask the bus for current shared state.
        bus.send(proto::sync_request(lww.origin()).as_bytes());
        *self.bus.lock().unwrap_or_else(|e| e.into_inner()) = Some(bus);
    }
}

/// Interpret a C string path from the shim, treating null/empty as absent.
///
/// # Safety
/// `path` must be null or a valid NUL-terminated string.
pub unsafe fn optional_path(path: *const c_char) -> Option<PathBuf> {
    if path.is_null() {
        return None;
    }
    let s = CStr::from_ptr(path).to_str().ok()?;
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// Generate the C ABI an `AUAudioUnit` subclass calls into.
///
/// The generated symbols are:
///
/// ```text
/// void*    skuiz_auv3_init(const char *app_group_dir);
/// void     skuiz_auv3_destroy(void *inst);
/// void     skuiz_auv3_activate(void *inst, double sample_rate, uint32_t max_frames);
/// void     skuiz_auv3_deactivate(void *inst);
/// uint32_t skuiz_auv3_audio_bus_count(uint8_t direction); // 0 = input, 1 = output
/// bool     skuiz_auv3_audio_bus_info(uint8_t direction, uint32_t index,
///                                    SkuizAudioBusInfo *out);
/// void     skuiz_auv3_render(void *inst, const SkuizAudioBusBuffers *inputs,
///                            const SkuizAudioBusBuffers *outputs, uint32_t frames);
/// uint32_t skuiz_auv3_param_count(void);
/// bool     skuiz_auv3_param_info(uint32_t index, SkuizParamInfo *out);
/// const char *skuiz_auv3_choice_label(uint32_t param_id, uint32_t choice_index);
/// double   skuiz_auv3_get_param(void *inst, uint32_t id);
/// void     skuiz_auv3_set_param(void *inst, uint32_t id, double value);
/// void     skuiz_auv3_set_param_from_render(void *inst, uint32_t id, double value);
/// uint32_t skuiz_auv3_save_state(void *inst, uint8_t *buf, uint32_t cap);
/// bool     skuiz_auv3_load_state(void *inst, const uint8_t *buf, uint32_t len);
/// void     skuiz_auv3_reset(void *inst);
/// uint32_t skuiz_auv3_midi_count(void *inst);
/// bool     skuiz_auv3_midi_event(void *inst, uint32_t index,
///                                uint32_t *frame, uint8_t *bytes3);
/// ```
#[macro_export]
macro_rules! export_auv3 {
    ($P:ty) => {
        const _: () = {
            use $crate::skuiz_core::Processor as _;
            type Inst = $crate::AuInstance<$P>;

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_init(
                app_group_dir: *const ::std::ffi::c_char,
            ) -> *mut ::std::ffi::c_void {
                $crate::ffi_guard(
                    || {
                        let inst = Box::new(Inst::new());
                        inst.join_bus($crate::optional_path(app_group_dir));
                        Box::into_raw(inst) as *mut ::std::ffi::c_void
                    },
                    ::std::ptr::null_mut(),
                )
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_destroy(inst: *mut ::std::ffi::c_void) {
                $crate::ffi_guard(
                    || {
                        if !inst.is_null() {
                            drop(Box::from_raw(inst as *mut Inst));
                        }
                    },
                    (),
                )
            }

            unsafe fn inst<'a>(ptr: *mut ::std::ffi::c_void) -> Option<&'a Inst> {
                (ptr as *const Inst).as_ref()
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_activate(
                ptr: *mut ::std::ffi::c_void,
                sample_rate: f64,
                max_frames: u32,
            ) {
                $crate::ffi_guard(
                    || {
                        let Some(i) = inst(ptr) else { return };
                        // `None` means blocks are already flowing; a host that
                        // re-activates without deactivating keeps the running
                        // setup rather than reconfiguring mid-stream.
                        let _ = i.engine.with_main(|core| {
                            core.processor.activate(sample_rate, max_frames);
                        });
                    },
                    (),
                )
            }

            /// The pair of `skuiz_auv3_activate`, called from the shim's
            /// `deallocateRenderResources`.
            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_deactivate(ptr: *mut ::std::ffi::c_void) {
                $crate::ffi_guard(
                    || {
                        let Some(i) = inst(ptr) else { return };
                        // Render released the audio state when the last
                        // block returned; hosts serialise this against
                        // render calls, so the direct path always applies.
                        let _ = i.engine.with_main(|core| {
                            core.processor.deactivate();
                        });
                    },
                    (),
                )
            }

            /// Number of declared buses in a direction (0 = input, 1 =
            /// output). The shim queries this at init to build its
            /// `AUAudioUnitBusArray`s.
            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_audio_bus_count(direction: u8) -> u32 {
                $crate::ffi_guard(
                    || {
                        let mut count = 0u32;
                        while $crate::bus_spec::<$P>(direction, count).is_some() {
                            count += 1;
                        }
                        count
                    },
                    0,
                )
            }

            /// Static description of one declared bus. Returns false for an
            /// out-of-range index.
            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_audio_bus_info(
                direction: u8,
                index: u32,
                out: *mut $crate::SkuizAudioBusInfo,
            ) -> bool {
                $crate::ffi_guard(
                    || {
                        let Some(spec) = $crate::bus_spec::<$P>(direction, index) else {
                            return false;
                        };
                        let Some(out) = out.as_mut() else { return false };
                        out.channel_count = spec.layout.channels() as u32;
                        out.optional = spec.optional as u8;
                        out.name = $crate::bus_name_ptr(spec.name);
                        true
                    },
                    false,
                )
            }

            /// Render one block (or one automation segment — the shim splits
            /// blocks at timed parameter events and offsets the pointers).
            ///
            /// `inputs`/`outputs` each point at
            /// [`MAX_BUSES_PER_DIRECTION`](skuiz_core::bus::MAX_BUSES_PER_DIRECTION)
            /// entries in declaration order; either may be null when the
            /// direction has no buses this block. The main input aliases the
            /// main output — the shim already pulled the upstream audio into
            /// the output buffers, which is the copy-in.
            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_render(
                ptr: *mut ::std::ffi::c_void,
                inputs: *const $crate::SkuizAudioBusBuffers,
                outputs: *const $crate::SkuizAudioBusBuffers,
                frames: u32,
            ) {
                $crate::ffi_guard(
                    || {
                        use $crate::skuiz_core::bus::{
                            BusDirection, MAX_BUS_CHANNELS, MAX_BUSES_PER_DIRECTION,
                        };

                        let Some(i) = inst(ptr) else { return };
                        // AUv3 has no start/stop_processing pair, so the
                        // audio state brackets each render call: claimed
                        // here, released at the bottom. Between calls the
                        // engine is IDLE, which keeps main-thread state ops
                        // (fullState) on the direct path — hosts routinely
                        // query state while no blocks are flowing.
                        if !i.engine.is_processing() {
                            i.audio_token.replace(Some(i.engine.begin_audio()));
                        }
                        let core = {
                            let token = i.audio_token.borrow();
                            i.engine
                                .audio_core(token.as_ref().expect("AUDIO implies a token"))
                        };
                        i.engine.drain_commands(core);

                        // The validated declaration the scratch was sized
                        // from; a `&'static` copy, so no borrow of `core`.
                        let specs = core.bus_scratch.specs();
                        let len = frames as usize;
                        let scratch = &mut core.bus_scratch;
                        scratch.clear();

                        // Outputs: each declared bus maps to the shim's slot
                        // of the same index (today only bus 0 is wired; AUv3
                        // hands the unit one output buffer list per render).
                        // The main bus's pointers are remembered for the
                        // main-input alias below.
                        let mut main_out: [*mut f32; MAX_BUS_CHANNELS] =
                            [::std::ptr::null_mut(); MAX_BUS_CHANNELS];
                        let mut main_out_n = 0usize;
                        if !outputs.is_null() && frames > 0 {
                            for (b, spec) in specs
                                .iter()
                                .filter(|s| s.direction == BusDirection::Output)
                                .enumerate()
                                .take(MAX_BUSES_PER_DIRECTION)
                            {
                                let bus = &*outputs.add(b);
                                if bus.active == 0 {
                                    continue;
                                }
                                let n = (bus.channel_count as usize)
                                    .min(spec.layout.channels() as usize);
                                if n == 0 {
                                    continue;
                                }
                                scratch.set_active(BusDirection::Output, b, true);
                                for (c, &p) in
                                    bus.channels.iter().enumerate().take(n)
                                {
                                    if p.is_null() {
                                        break;
                                    }
                                    scratch.set_channel(BusDirection::Output, b, c, p, len);
                                    if b == 0 {
                                        main_out[c] = p;
                                        main_out_n = c + 1;
                                    }
                                }
                            }
                        }

                        // Inputs: the main bus aliases the main output (the
                        // pull already landed there); any further declared
                        // input is a read-only sidechain, active only when
                        // the host connected it — absence is not an error.
                        for (b, spec) in specs
                            .iter()
                            .filter(|s| s.direction == BusDirection::Input)
                            .enumerate()
                            .take(MAX_BUSES_PER_DIRECTION)
                        {
                            if b == 0 {
                                if main_out_n == 0 {
                                    continue;
                                }
                                scratch.set_active(BusDirection::Input, 0, true);
                                for (c, &p) in
                                    main_out.iter().enumerate().take(main_out_n)
                                {
                                    scratch.set_channel(BusDirection::Input, 0, c, p, len);
                                }
                                continue;
                            }
                            if inputs.is_null() || frames == 0 {
                                continue;
                            }
                            let bus = &*inputs.add(b);
                            if bus.active == 0 {
                                continue;
                            }
                            let n = (bus.channel_count as usize)
                                .min(spec.layout.channels() as usize);
                            if n == 0 {
                                continue;
                            }
                            scratch.set_active(BusDirection::Input, b, true);
                            for (c, &p) in bus.channels.iter().enumerate().take(n) {
                                if p.is_null() {
                                    break;
                                }
                                scratch.set_channel(BusDirection::Input, b, c, p, len);
                            }
                        }

                        core.midi_out.clear();
                        let (inputs, mut outputs) = scratch.views();
                        core.processor.process(&inputs, &mut outputs, &mut core.midi_out);
                        i.engine.diag().midi_events_dropped.fetch_add(
                            core.midi_out.dropped() as u64,
                            ::std::sync::atomic::Ordering::Relaxed,
                        );
                        if let Some(token) = i.audio_token.take() {
                            i.engine.end_audio(token);
                        }
                    },
                    (),
                )
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_param_count() -> u32 {
                <$P>::params().len() as u32
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_param_info(
                index: u32,
                out: *mut $crate::SkuizParamInfo,
            ) -> bool {
                let Some(def) = <$P>::params().get(index as usize) else { return false };
                let Some(out) = out.as_mut() else { return false };
                out.id = def.id;
                out.name = $crate::name_ptr::<$P>(def.id);
                out.min = def.low();
                out.max = def.high();
                out.default = def.default;
                out.choice_count = def.choices.len() as u32;
                true
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_choice_label(
                param_id: u32,
                choice_index: u32,
            ) -> *const ::std::ffi::c_char {
                $crate::choice_label_ptr::<$P>(param_id, choice_index)
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_get_param(
                ptr: *mut ::std::ffi::c_void,
                id: u32,
            ) -> f64 {
                $crate::ffi_guard(
                    || {
                        let Some(i) = inst(ptr) else { return 0.0 };
                        // The mirror answers reads wait-free (invariant 6);
                        // a param the mirror doesn't know is not ours.
                        i.engine.mirror().get(id).unwrap_or(0.0)
                    },
                    0.0,
                )
            }

            /// Main/UI thread only: broadcasts to other instances, which
            /// allocates and writes to a socket. The render thread must use
            /// `skuiz_auv3_set_param_from_render` instead.
            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_set_param(
                ptr: *mut ::std::ffi::c_void,
                id: u32,
                value: f64,
            ) {
                $crate::ffi_guard(
                    || {
                        let Some(i) = inst(ptr) else { return };
                        let Some(def) = <$P>::params().iter().find(|d| d.id == id) else {
                            return;
                        };
                        // Direct when the transport is stopped, queued for
                        // the next block when running; never locks the
                        // processor. For shared parameters the apply happens
                        // inside `stamp_with`: only a change that entered
                        // the engine claims a version and reaches the bus
                        // (invariant 9).
                        if def.shared {
                            // Share the move with every other instance on the
                            // bus. The versioned frame lets receivers discard
                            // stale echoes (invariant 9).
                            let stamped = i.lww.stamp_with(id, || i.engine.set_param(id, value));
                            if let Some((seq, origin)) = stamped {
                                if let Some(bus) =
                                    i.bus.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
                                {
                                    bus.send(
                                        $crate::skuiz_core::protocol::set_param_versioned(
                                            id, value, seq, origin,
                                        )
                                        .as_bytes(),
                                    );
                                }
                            }
                        } else {
                            // Local parameters stay in the instance
                            // (invariant 10).
                            i.engine.set_param(id, value);
                        }
                    },
                    (),
                )
            }

            /// Render-thread parameter application: no allocation, no bus.
            ///
            /// The shim calls this between segments of a split block, where
            /// the engine is momentarily idle, so the change applies
            /// directly through the engine's main access — a CAS, never a
            /// mutex (invariant 2). Host automation is deliberately not
            /// broadcast — matching the CLAP adapter, where only editor
            /// moves cross the bus — and a socket write has no place on the
            /// render thread anyway.
            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_set_param_from_render(
                ptr: *mut ::std::ffi::c_void,
                id: u32,
                value: f64,
            ) {
                $crate::ffi_guard(
                    || {
                        let Some(i) = inst(ptr) else { return };
                        if !<$P>::params().iter().any(|d| d.id == id) {
                            return;
                        }
                        i.engine.set_param(id, value);
                    },
                    (),
                )
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_save_state(
                ptr: *mut ::std::ffi::c_void,
                buf: *mut u8,
                cap: u32,
            ) -> u32 {
                $crate::ffi_guard(
                    || {
                        let Some(i) = inst(ptr) else { return 0 };
                        // Direct when stopped, a bounded round-trip through
                        // the render thread when running; `None` means the
                        // round-trip timed out.
                        let Some(data) = i.engine.save_state() else { return 0 };
                        // A null buffer (or one too small) reports the size needed,
                        // so the shim can allocate and call again.
                        if buf.is_null() || (cap as usize) < data.len() {
                            return data.len() as u32;
                        }
                        ::std::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len());
                        data.len() as u32
                    },
                    0,
                )
            }

            /// Reset DSP state between blocks, from the shim's `reset`
            /// override. Hosts may call this with the transport in any
            /// state: direct when idle, queued for the next block top when
            /// rendering.
            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_reset(ptr: *mut ::std::ffi::c_void) {
                $crate::ffi_guard(
                    || {
                        let Some(i) = inst(ptr) else { return };
                        i.engine.reset();
                    },
                    (),
                )
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_load_state(
                ptr: *mut ::std::ffi::c_void,
                buf: *const u8,
                len: u32,
            ) -> bool {
                $crate::ffi_guard(
                    || {
                        let Some(i) = inst(ptr) else { return false };
                        if buf.is_null() {
                            return false;
                        }
                        let data = ::std::slice::from_raw_parts(buf, len as usize);
                        let ok = i.engine.load_state(data.to_vec());
                        if ok {
                            // Project state replaced the parameter values
                            // without versions: stop advertising them to the
                            // bus until a shared edit lands (invariant 10).
                            i.lww.on_state_load();
                        }
                        ok
                    },
                    false,
                )
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_midi_count(
                ptr: *mut ::std::ffi::c_void,
            ) -> u32 {
                $crate::ffi_guard(
                    || {
                        let Some(i) = inst(ptr) else { return 0 };
                        // The shim pulls events from inside the render block,
                        // right after `skuiz_auv3_render` returned — at which
                        // point that call already handed its token back, so
                        // the engine is IDLE. Re-claim the audio state for
                        // the read: `midi_out`'s single writer is this same
                        // render thread (see `Engine::midi_out`), and
                        // `begin_audio` can only spin out an in-flight main
                        // access, which is always brief.
                        // This ABI carries 3-byte MIDI 1.0 only: UMP-only
                        // (MIDI 2.0) events are invisible here. Widen the C
                        // ABI when a plugin needs MIDI 2.0 out of AUv3.
                        let token = i.engine.begin_audio();
                        let count = i
                            .engine
                            .midi_out(&token)
                            .events()
                            .iter()
                            .filter(|(_, ev)| ev.midi1_bytes().is_some())
                            .count() as u32;
                        i.engine.end_audio(token);
                        count
                    },
                    0,
                )
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_midi_event(
                ptr: *mut ::std::ffi::c_void,
                index: u32,
                frame: *mut u32,
                bytes3: *mut u8,
            ) -> bool {
                $crate::ffi_guard(
                    || {
                        let Some(i) = inst(ptr) else { return false };
                        if frame.is_null() || bytes3.is_null() {
                            return false;
                        }
                        // Same re-claim as `skuiz_auv3_midi_count`: this runs
                        // inside the render block, after render released the
                        // audio state.
                        let token = i.engine.begin_audio();
                        let midi = i.engine.midi_out(&token);
                        // `index` counts only MIDI 1.0 events, matching
                        // `skuiz_auv3_midi_count`; UMP-only events are skipped.
                        let mut seen = 0u32;
                        let mut found = false;
                        for &(at, ev) in midi.events() {
                            let Some(data) = ev.midi1_bytes() else {
                                continue;
                            };
                            if seen == index {
                                *frame = at;
                                ::std::ptr::copy_nonoverlapping(data.as_ptr(), bytes3, 3);
                                found = true;
                                break;
                            }
                            seen += 1;
                        }
                        i.engine.end_audio(token);
                        found
                    },
                    false,
                )
            }
        };
    };
}

/// Cache of leaked NUL-terminated strings, keyed per plugin type. The leak
/// is bounded: entries only exist for `&'static` parameter metadata.
type CStrCache<K> = Mutex<std::collections::HashMap<K, &'static CStr>>;

/// NUL-terminated parameter name for the C side.
///
/// Parameter names are `&'static str`, so the leaked `CString` is a
/// one-time, bounded cost rather than a growing leak.
pub fn name_ptr<P: Processor>(id: u32) -> *const c_char {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static NAMES: OnceLock<CStrCache<(usize, u32)>> = OnceLock::new();
    let map = NAMES.get_or_init(|| Mutex::new(HashMap::new()));

    let key = (P::params().as_ptr() as usize, id);
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(found) = map.get(&key) {
        return found.as_ptr();
    }
    let name = P::params()
        .iter()
        .find(|p| p.id == id)
        .map_or("", |p| p.name);
    let owned: &'static CStr = Box::leak(
        std::ffi::CString::new(name)
            .unwrap_or_default()
            .into_boxed_c_str(),
    );
    map.insert(key, owned);
    owned.as_ptr()
}

/// NUL-terminated choice label, or null when the index is out of range.
pub fn choice_label_ptr<P: Processor>(param_id: u32, choice_index: u32) -> *const c_char {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static LABELS: OnceLock<CStrCache<(usize, u32, u32)>> = OnceLock::new();
    let map = LABELS.get_or_init(|| Mutex::new(HashMap::new()));

    let Some(def) = P::params().iter().find(|p| p.id == param_id) else {
        return std::ptr::null();
    };
    let Some(label) = def.choices.get(choice_index as usize) else {
        return std::ptr::null();
    };

    let key = (P::params().as_ptr() as usize, param_id, choice_index);
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(found) = map.get(&key) {
        return found.as_ptr();
    }
    let owned: &'static CStr = Box::leak(
        std::ffi::CString::new(*label)
            .unwrap_or_default()
            .into_boxed_c_str(),
    );
    map.insert(key, owned);
    owned.as_ptr()
}

pub use skuiz_core;
