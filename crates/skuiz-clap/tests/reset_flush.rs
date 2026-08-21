//! CLAP vtable regression tests: `reset` must reach `Processor::reset`
//! (stopped and running), and `params_flush` must apply its events on both
//! spec threading paths without ever losing them silently.

use clap_sys::events::{
    clap_event_header, clap_event_param_value, clap_input_events, CLAP_CORE_EVENT_SPACE_ID,
    CLAP_EVENT_PARAM_VALUE,
};
use clap_sys::ext::params::{clap_plugin_params, CLAP_EXT_PARAMS};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::plugin::clap_plugin;
use clap_sys::process::clap_process;
use clap_sys::stream::clap_istream;
use skuiz_clap::ClapDescriptor;
use skuiz_core::{ParamDef, PluginInfo, Processor};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static RESETS: AtomicUsize = AtomicUsize::new(0);
/// Holds the contended-flush test's state load open until released.
static LOAD_GATE: AtomicBool = AtomicBool::new(false);

struct Gain(f64);

impl Default for Gain {
    fn default() -> Self {
        Self(1.0)
    }
}

impl Processor for Gain {
    fn info() -> PluginInfo {
        // Distinct id => distinct bus scope; keeps other tests out.
        PluginInfo {
            id: "test.resetflush",
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
    fn reset(&mut self) {
        RESETS.fetch_add(1, Ordering::SeqCst);
    }
    fn load_state(&mut self, data: &[u8]) -> bool {
        // Only the contended-flush test closes the gate; for every other
        // test this is an ordinary load.
        if LOAD_GATE.load(Ordering::SeqCst) {
            IN_GATE.store(true, Ordering::SeqCst);
            while LOAD_GATE.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        }
        let Some(pair) = data.strip_prefix(skuiz_core::STATE_MAGIC) else {
            return false;
        };
        if pair.len() == 12 {
            self.0 = f64::from_le_bytes(pair[4..12].try_into().unwrap());
        }
        true
    }
    fn process(&mut self, _channels: &mut [&mut [f32]], _midi: &mut skuiz_core::MidiOut) {}
}

unsafe fn ext<T>(plugin: *const clap_plugin, id: &std::ffi::CStr) -> &'static T {
    let p = ((*plugin).get_extension.unwrap())(plugin, id.as_ptr());
    assert!(!p.is_null());
    &*(p as *const T)
}

unsafe fn param_value(plugin: *const clap_plugin) -> f64 {
    let params: &clap_plugin_params = ext(plugin, CLAP_EXT_PARAMS);
    let mut v = f64::NAN;
    assert!((params.get_value.unwrap())(plugin, 0, &mut v));
    v
}

unsafe fn run_empty_process(plugin: *const clap_plugin) {
    let p = clap_process {
        steady_time: 0,
        frames_count: 0,
        transport: null(),
        audio_inputs: null(),
        audio_outputs: null_mut(),
        audio_inputs_count: 0,
        audio_outputs_count: 0,
        in_events: null(),
        out_events: null(),
    };
    ((*plugin).process.unwrap())(plugin, &p);
}

fn param_ev(id: u32, value: f64) -> clap_event_param_value {
    clap_event_param_value {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_param_value>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_VALUE,
            flags: 0,
        },
        param_id: id,
        cookie: null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value,
    }
}

unsafe extern "C" fn ev_size(list: *const clap_input_events) -> u32 {
    (*((*list).ctx as *const Vec<clap_event_param_value>)).len() as u32
}

unsafe extern "C" fn ev_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    let events = &*((*list).ctx as *const Vec<clap_event_param_value>);
    &events[index as usize].header
}

/// Call `params_flush` with one event list.
unsafe fn flush(plugin: *const clap_plugin, events: &Vec<clap_event_param_value>) {
    let params: &clap_plugin_params = ext(plugin, CLAP_EXT_PARAMS);
    let list = clap_input_events {
        ctx: events as *const _ as *mut c_void,
        size: Some(ev_size),
        get: Some(ev_get),
    };
    (params.flush.unwrap())(plugin, &list, null_mut());
}

#[test]
fn reset_reaches_the_processor_stopped_and_running() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<Gain>()));
        let plugin = skuiz_clap::instantiate::<Gain>(&desc.raw, null());
        assert!(((*plugin).init.unwrap())(plugin));

        let before = RESETS.load(Ordering::SeqCst);
        ((*plugin).reset.unwrap())(plugin);
        assert_eq!(
            RESETS.load(Ordering::SeqCst),
            before + 1,
            "stopped: reset applies directly"
        );

        assert!(((*plugin).activate.unwrap())(plugin, 48_000.0, 64, 512));
        assert!(((*plugin).start_processing.unwrap())(plugin));
        ((*plugin).reset.unwrap())(plugin);
        assert_eq!(
            RESETS.load(Ordering::SeqCst),
            before + 1,
            "running: reset waits for a block boundary"
        );
        run_empty_process(plugin);
        assert_eq!(
            RESETS.load(Ordering::SeqCst),
            before + 2,
            "running: reset landed at the top of the next block"
        );

        ((*plugin).stop_processing.unwrap())(plugin);
        ((*plugin).deactivate.unwrap())(plugin);
        ((*plugin).destroy.unwrap())(plugin);
    }
}

/// Stopped flush: [main-thread & !processing] — values apply directly and
/// publish to the mirror.
#[test]
fn flush_applies_while_stopped() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<Gain>()));
        let plugin = skuiz_clap::instantiate::<Gain>(&desc.raw, null());
        assert!(((*plugin).init.unwrap())(plugin));

        flush(plugin, &vec![param_ev(0, 0.25)]);
        assert_eq!(param_value(plugin), 0.25);

        ((*plugin).destroy.unwrap())(plugin);
    }
}

/// Flush while processing: [audio-thread & processing] — same application
/// rules as process().
#[test]
fn flush_applies_while_processing() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<Gain>()));
        let plugin = skuiz_clap::instantiate::<Gain>(&desc.raw, null());
        assert!(((*plugin).init.unwrap())(plugin));
        assert!(((*plugin).activate.unwrap())(plugin, 48_000.0, 64, 512));
        assert!(((*plugin).start_processing.unwrap())(plugin));

        flush(plugin, &vec![param_ev(0, 0.6)]);
        assert_eq!(param_value(plugin), 0.6);

        ((*plugin).stop_processing.unwrap())(plugin);
        ((*plugin).deactivate.unwrap())(plugin);
        ((*plugin).destroy.unwrap())(plugin);
    }
}

/// Flush while another thread holds a stopped main access (a slow state
/// load): the events must not vanish silently into the contended access.
/// They are routed through the engine, which refuses them counted
/// (invariant 8) rather than parking them unseen; once the access clears,
/// a retry lands normally and nothing is wedged.
#[test]
fn flush_during_a_state_load_neither_wedges_nor_lies() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<Gain>()));
        let plugin = skuiz_clap::instantiate::<Gain>(&desc.raw, null());
        assert!(((*plugin).init.unwrap())(plugin));

        // A state load whose processor hook blocks until the gate opens:
        // the main state is held for its duration.
        LOAD_GATE.store(true, Ordering::SeqCst);
        IN_GATE.store(false, Ordering::SeqCst);
        let loader = std::thread::spawn({
            let plugin = SendPtr(plugin);
            move || {
                let plugin = plugin.into_inner();
                let mut data = skuiz_core::STATE_MAGIC.to_vec();
                data.extend_from_slice(&0u32.to_le_bytes());
                data.extend_from_slice(&0.5f64.to_le_bytes());
                let istream = clap_istream {
                    ctx: &mut data as *mut _ as *mut c_void,
                    read: Some(read_all),
                };
                let state: &clap_plugin_state = ext(plugin, CLAP_EXT_STATE);
                (state.load.unwrap())(plugin, &istream)
            }
        });

        // Wait until the loader is actually inside the gated load.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !loader_in_gate() {
            assert!(Instant::now() < deadline, "loader never entered the gate");
            std::thread::yield_now();
        }

        // Flush against the held main state: bounded, then counted as
        // dropped — the value must NOT appear, or the flush would have
        // reported success for a change nobody applied.
        flush(plugin, &vec![param_ev(0, 0.9)]);
        assert_eq!(
            param_value(plugin),
            1.0,
            "a refused flush must not half-apply"
        );

        // Let the load finish; the engine is healthy afterwards.
        LOAD_GATE.store(false, Ordering::SeqCst);
        assert!(loader.join().unwrap(), "the gated load itself failed");
        assert_eq!(param_value(plugin), 0.5);

        flush(plugin, &vec![param_ev(0, 0.9)]);
        assert_eq!(param_value(plugin), 0.9, "a later flush lands normally");

        ((*plugin).destroy.unwrap())(plugin);
    }
}

/// Set inside the gated load so the test can wait for the exact window.
static IN_GATE: AtomicBool = AtomicBool::new(false);

fn loader_in_gate() -> bool {
    IN_GATE.load(Ordering::SeqCst)
}

/// Raw plugin pointers are not Send; this newtype moves one to the loader
/// thread. Sound here: that thread only calls the main-thread state entry
/// point while the test's own thread refrains from touching the plugin
/// until the flush below, which is precisely the contention under test.
struct SendPtr<T>(T);
unsafe impl<T> Send for SendPtr<T> {}

impl<T> SendPtr<T> {
    /// A method, not a field access, so closures capture the Send wrapper
    /// whole instead of the raw pointer field inside it.
    fn into_inner(self) -> T {
        self.0
    }
}

unsafe extern "C" fn read_all(stream: *const clap_istream, buffer: *mut c_void, size: u64) -> i64 {
    let data = &mut *((*stream).ctx as *mut Vec<u8>);
    let n = (size as usize).min(data.len());
    if n == 0 {
        return 0;
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), buffer as *mut u8, n);
    data.drain(..n);
    n as i64
}
