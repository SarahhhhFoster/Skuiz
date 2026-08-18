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

use skuiz_core::{MidiOut, Processor};
use std::ffi::{c_char, CStr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Events one block may emit before further MIDI is dropped.
const MIDI_OUT_CAPACITY: usize = 512;

/// Parameter description handed to the Objective-C side, which turns each
/// one into an `AUParameter`. Choice parameters arrive with
/// `choice_count > 0`; fetch their labels with `skuiz_auv3_choice_label`.
#[repr(C)]
pub struct SkuizParamInfo {
    pub id: u32,
    /// NUL-terminated, owned by Rust and valid for the process lifetime
    /// (parameter names are `&'static str`).
    pub name: *const c_char,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub choice_count: u32,
}

/// The live plugin instance behind the opaque pointer the shim holds.
pub struct AuInstance<P: Processor> {
    pub processor: Mutex<P>,
    pub midi_out: Mutex<MidiOut>,
    pub bus: Mutex<Option<skuiz_ipc::Bus>>,
    pub sync: Arc<SyncState>,
}

#[derive(Default)]
pub struct SyncState {
    pub pending: Mutex<Vec<(u32, f64)>>,
}

impl<P: Processor + Default> Default for AuInstance<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: Processor + Default> AuInstance<P> {
    pub fn new() -> Self {
        Self {
            processor: Mutex::new(P::default()),
            midi_out: Mutex::new(MidiOut::with_capacity(MIDI_OUT_CAPACITY)),
            bus: Mutex::new(None),
            sync: Arc::new(SyncState::default()),
        }
    }

    /// Join the IPC bus, placing the socket in `group_dir` when the host
    /// sandbox requires it.
    pub fn join_bus(&self, group_dir: Option<PathBuf>) {
        let sync = Arc::clone(&self.sync);
        let on_message = move |frame: &[u8]| {
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
            sync.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((id, value));
        };
        let bus = match group_dir {
            Some(dir) => skuiz_ipc::Bus::join_in(&dir, P::info().id, on_message),
            None => skuiz_ipc::Bus::join(P::info().id, on_message),
        };
        *self.bus.lock().unwrap_or_else(|e| e.into_inner()) = Some(bus);
    }

    /// Drain parameter values that arrived over IPC into the processor.
    /// Called at the top of each render block.
    pub fn drain_remote_params(&self) {
        let Ok(mut pending) = self.sync.pending.lock() else {
            return;
        };
        if pending.is_empty() {
            return;
        }
        if let Ok(mut p) = self.processor.lock() {
            for (id, value) in pending.drain(..) {
                if P::params().iter().any(|d| d.id == id) {
                    p.set_param(id, value);
                }
            }
        }
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
/// void     skuiz_auv3_render(void *inst, float *const *channels,
///                            uint32_t channel_count, uint32_t frames);
/// uint32_t skuiz_auv3_param_count(void);
/// bool     skuiz_auv3_param_info(uint32_t index, SkuizParamInfo *out);
/// const char *skuiz_auv3_choice_label(uint32_t param_id, uint32_t choice_index);
/// double   skuiz_auv3_get_param(void *inst, uint32_t id);
/// void     skuiz_auv3_set_param(void *inst, uint32_t id, double value);
/// void     skuiz_auv3_set_param_from_render(void *inst, uint32_t id, double value);
/// uint32_t skuiz_auv3_save_state(void *inst, uint8_t *buf, uint32_t cap);
/// bool     skuiz_auv3_load_state(void *inst, const uint8_t *buf, uint32_t len);
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
                let inst = Box::new(Inst::new());
                inst.join_bus($crate::optional_path(app_group_dir));
                Box::into_raw(inst) as *mut ::std::ffi::c_void
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_destroy(inst: *mut ::std::ffi::c_void) {
                if !inst.is_null() {
                    drop(Box::from_raw(inst as *mut Inst));
                }
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
                let Some(i) = inst(ptr) else { return };
                if let Ok(mut p) = i.processor.lock() {
                    p.activate(sample_rate, max_frames);
                }
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_render(
                ptr: *mut ::std::ffi::c_void,
                channels: *const *mut f32,
                channel_count: u32,
                frames: u32,
            ) {
                let Some(i) = inst(ptr) else { return };
                i.drain_remote_params();

                let n_ch = (channel_count as usize).min(2);
                let mut chans: [&mut [f32]; 2] = [&mut [], &mut []];
                let mut used = 0;
                if !channels.is_null() && frames > 0 {
                    for c in 0..n_ch {
                        let p = *channels.add(c);
                        if p.is_null() {
                            break;
                        }
                        chans[c] = ::std::slice::from_raw_parts_mut(p, frames as usize);
                        used += 1;
                    }
                }

                let Ok(mut midi) = i.midi_out.lock() else { return };
                midi.clear();
                if let Ok(mut p) = i.processor.lock() {
                    p.process(&mut chans[..used], &mut midi);
                }
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
                let Some(i) = inst(ptr) else { return 0.0 };
                i.processor.lock().map(|p| p.get_param(id)).unwrap_or(0.0)
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
                let Some(i) = inst(ptr) else { return };
                if !<$P>::params().iter().any(|d| d.id == id) {
                    return;
                }
                if let Ok(mut p) = i.processor.lock() {
                    p.set_param(id, value);
                }
                // Share the move with every other instance on the bus.
                if let Ok(bus) = i.bus.lock() {
                    if let Some(bus) = bus.as_ref() {
                        bus.send(
                            $crate::skuiz_core::protocol::set_param(id, value).as_bytes(),
                        );
                    }
                }
            }

            /// Render-thread parameter application: no allocation, no bus.
            ///
            /// Host automation is deliberately not broadcast — matching the
            /// CLAP adapter, where only editor moves cross the bus — and a
            /// socket write has no place on the render thread anyway.
            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_set_param_from_render(
                ptr: *mut ::std::ffi::c_void,
                id: u32,
                value: f64,
            ) {
                let Some(i) = inst(ptr) else { return };
                if !<$P>::params().iter().any(|d| d.id == id) {
                    return;
                }
                if let Ok(mut p) = i.processor.lock() {
                    p.set_param(id, value);
                }
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_save_state(
                ptr: *mut ::std::ffi::c_void,
                buf: *mut u8,
                cap: u32,
            ) -> u32 {
                let Some(i) = inst(ptr) else { return 0 };
                let Ok(p) = i.processor.lock() else { return 0 };
                let data = p.save_state();
                // A null buffer (or one too small) reports the size needed,
                // so the shim can allocate and call again.
                if buf.is_null() || (cap as usize) < data.len() {
                    return data.len() as u32;
                }
                ::std::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len());
                data.len() as u32
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_load_state(
                ptr: *mut ::std::ffi::c_void,
                buf: *const u8,
                len: u32,
            ) -> bool {
                let Some(i) = inst(ptr) else { return false };
                if buf.is_null() {
                    return false;
                }
                let data = ::std::slice::from_raw_parts(buf, len as usize);
                match i.processor.lock() {
                    Ok(mut p) => p.load_state(data),
                    Err(_) => false,
                }
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_midi_count(
                ptr: *mut ::std::ffi::c_void,
            ) -> u32 {
                let Some(i) = inst(ptr) else { return 0 };
                i.midi_out.lock().map(|m| m.events().len() as u32).unwrap_or(0)
            }

            #[no_mangle]
            pub unsafe extern "C" fn skuiz_auv3_midi_event(
                ptr: *mut ::std::ffi::c_void,
                index: u32,
                frame: *mut u32,
                bytes3: *mut u8,
            ) -> bool {
                let Some(i) = inst(ptr) else { return false };
                if frame.is_null() || bytes3.is_null() {
                    return false;
                }
                let Ok(midi) = i.midi_out.lock() else { return false };
                let Some(&(at, data)) = midi.events().get(index as usize) else {
                    return false;
                };
                *frame = at;
                ::std::ptr::copy_nonoverlapping(data.as_ptr(), bytes3, 3);
                true
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
