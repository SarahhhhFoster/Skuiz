//! RT-hammer: hostile concurrency against the engine's threading model.
//! One instance is activated and started, then attacked from three sides:
//! an audio thread pumping process(), the main thread reading values and
//! round-tripping state through CLAP_EXT_STATE, and a bus node spamming
//! set_param frames. The engine must survive without deadlock or panic,
//! and the final value must be one that was actually sent.
//!
//! CLAP has no main-thread `set_value`: while processing runs, `flush` is
//! an audio-thread-only call, so host-side changes are injected over the
//! bus instead — the same path a remote editor takes.

use clap_sys::ext::params::{clap_plugin_params, CLAP_EXT_PARAMS};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::plugin::clap_plugin;
use clap_sys::process::clap_process;
use clap_sys::stream::{clap_istream, clap_ostream};
use skuiz_clap::ClapDescriptor;
use skuiz_core::{ParamDef, PluginInfo, Processor};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Gain(f64);

impl Default for Gain {
    fn default() -> Self {
        Self(1.0)
    }
}

impl Processor for Gain {
    fn info() -> PluginInfo {
        // Distinct id => distinct bus socket; keeps other tests out.
        PluginInfo {
            id: "test.claphammer",
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
    fn process(&mut self, _channels: &mut [&mut [f32]], _midi: &mut skuiz_core::MidiOut) {}
}

/// Every value any thread ever sends. The default (1.0) is a candidate too,
/// so "the mirror holds a candidate" holds even before anything lands.
/// Candidates survive the protocol's shortest-round-trip text formatting
/// exactly, so membership can be tested by equality (with epsilon slack).
fn candidate(i: usize) -> f64 {
    (i % 8) as f64 / 7.0
}

fn is_candidate(v: f64) -> bool {
    (0..8).any(|k| (v - candidate(k)).abs() < 1e-12)
}

/// Raw plugin pointers are not Send; this newtype moves one to the single
/// audio thread. Sound here: only that thread calls process(); the main
/// thread sticks to main-thread-safe entry points (mirror reads, state ops).
struct SendPtr<T>(T);
unsafe impl<T> Send for SendPtr<T> {}

impl<T> SendPtr<T> {
    /// A method, not a field access or destructure, so closures capture the
    /// Send wrapper whole instead of the raw pointer field inside it.
    fn into_inner(self) -> T {
        self.0
    }
}

unsafe fn run_block(plugin: *const clap_plugin, out: &mut [*mut f32; 2]) {
    let mut out_buf = clap_sys::audio_buffer::clap_audio_buffer {
        data32: out.as_mut_ptr(),
        data64: null_mut(),
        channel_count: 2,
        latency: 0,
        constant_mask: 0,
    };
    let p = clap_process {
        steady_time: 0,
        frames_count: 64,
        transport: null(),
        audio_inputs: null(),
        audio_outputs: &mut out_buf,
        audio_inputs_count: 0,
        audio_outputs_count: 1,
        in_events: null(),
        out_events: null(),
    };
    ((*plugin).process.unwrap())(plugin, &p);
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

// ostream appending to a Vec<u8>, one byte at a time (worst case)
unsafe extern "C" fn sink_write(
    stream: *const clap_ostream,
    buffer: *const c_void,
    size: u64,
) -> i64 {
    if size == 0 {
        return 0;
    }
    let vec = &mut *((*stream).ctx as *mut Vec<u8>);
    vec.push(*(buffer as *const u8));
    1
}

// istream draining a Vec<u8>, one byte at a time (worst case)
unsafe extern "C" fn source_read(
    stream: *const clap_istream,
    buffer: *mut c_void,
    size: u64,
) -> i64 {
    let vec = &mut *((*stream).ctx as *mut Vec<u8>);
    if size == 0 || vec.is_empty() {
        return 0;
    }
    *(buffer as *mut u8) = vec.remove(0);
    1
}

/// Poll `cond` rather than sleep a fixed time: the suite runs in parallel,
/// so fixed delays are either flaky under load or needlessly slow.
fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

/// Sets `stop` even if the main thread panics mid-hammer, so the worker
/// threads wind down instead of spinning to their iteration caps.
struct StopGuard(Arc<AtomicBool>);
impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[test]
fn rt_hammer() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<Gain>()));
        let plugin = skuiz_clap::instantiate::<Gain>(&desc.raw, null());
        assert!(((*plugin).init.unwrap())(plugin));
        assert!(((*plugin).activate.unwrap())(plugin, 48_000.0, 64, 512));
        assert!(((*plugin).start_processing.unwrap())(plugin));

        let stop = Arc::new(AtomicBool::new(false));
        let _guard = StopGuard(Arc::clone(&stop));
        let audio_done = Arc::new(AtomicBool::new(false));
        let bus_done = Arc::new(AtomicBool::new(false));

        // Thread A (audio): pump blocks until the main thread says stop.
        // The cap is a backstop so a failed test still terminates.
        let audio = {
            let plugin = SendPtr(plugin as *mut c_void);
            let stop = Arc::clone(&stop);
            let audio_done = Arc::clone(&audio_done);
            std::thread::spawn(move || {
                let plugin = plugin.into_inner() as *const clap_plugin;
                let mut left = [0.0f32; 64];
                let mut right = [0.0f32; 64];
                for _ in 0..500_000 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    run_block(plugin, &mut [left.as_mut_ptr(), right.as_mut_ptr()]);
                }
                audio_done.store(true, Ordering::Relaxed);
            })
        };

        // Thread B: a hostile bus node spamming set_param frames.
        let bus = {
            let bus_done = Arc::clone(&bus_done);
            std::thread::spawn(move || {
                let node = skuiz_ipc::Bus::join("test.claphammer", |_| {});
                for i in 0..2_000 {
                    node.send(skuiz_core::protocol::set_param(0, candidate(i)).as_bytes());
                }
                drop(node);
                bus_done.store(true, Ordering::Relaxed);
            })
        };

        // Main thread: reads plus its own bus writes, and a state
        // round-trip every few iterations. State ops while running are a
        // bounded round-trip through the audio thread — they only complete
        // because thread A keeps the blocks flowing.
        let state: &clap_plugin_state = ext(plugin, CLAP_EXT_STATE);
        let main_node = skuiz_ipc::Bus::join("test.claphammer", |_| {});
        for i in 0..300 {
            main_node.send(skuiz_core::protocol::set_param(0, candidate(i + 3)).as_bytes());
            let v = param_value(plugin);
            assert!(
                v.is_finite() && (0.0..=1.0).contains(&v),
                "mirror read out of range: {v}"
            );
            if i % 15 == 0 {
                let mut saved: Vec<u8> = Vec::new();
                let ostream = clap_ostream {
                    ctx: &mut saved as *mut _ as *mut c_void,
                    write: Some(sink_write),
                };
                assert!(
                    (state.save.unwrap())(plugin, &ostream),
                    "state save failed while blocks were flowing"
                );
                assert!(!saved.is_empty());
                let istream = clap_istream {
                    ctx: &mut saved as *mut _ as *mut c_void,
                    read: Some(source_read),
                };
                assert!(
                    (state.load.unwrap())(plugin, &istream),
                    "state load failed while blocks were flowing"
                );
            }
        }
        drop(main_node);

        // Everyone must finish; join before asserting so the threads are
        // reaped (and their panics propagated) even on the failure path.
        stop.store(true, Ordering::Relaxed);
        wait_until("hammer threads to finish", || {
            audio_done.load(Ordering::Relaxed) && bus_done.load(Ordering::Relaxed)
        });
        audio.join().unwrap();
        bus.join().unwrap();

        // Queued commands may still be in flight; pump a few final blocks
        // (single-threaded now) so the mirror reflects the last applied one.
        let mut left = [0.0f32; 64];
        let mut right = [0.0f32; 64];
        for _ in 0..8 {
            run_block(plugin, &mut [left.as_mut_ptr(), right.as_mut_ptr()]);
        }
        wait_until("param to converge to a sent value", || {
            is_candidate(param_value(plugin))
        });

        ((*plugin).stop_processing.unwrap())(plugin);
        ((*plugin).deactivate.unwrap())(plugin);
        ((*plugin).destroy.unwrap())(plugin);
    }
}
