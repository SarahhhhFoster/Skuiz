//! RT-hammer: param sets, state save/load and bus spam thrown at an instance
//! while a render thread pulls blocks as fast as it can. Proves the engine's
//! thread split holds under hostile concurrency: no deadlock, no panic, and
//! reads always answer with a real value.

use skuiz_core::{MidiOut, ParamDef, PluginInfo, Processor};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

struct Gain(f64);

impl Default for Gain {
    fn default() -> Self {
        Self(1.0)
    }
}

impl Processor for Gain {
    fn info() -> PluginInfo {
        PluginInfo {
            // Distinct id => distinct bus socket; keeps other tests out.
            id: "test.auv3hammer",
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
            max: 2.0,
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
    fn process(&mut self, channels: &mut [&mut [f32]], _midi: &mut MidiOut) {
        let g = self.0 as f32;
        for ch in channels.iter_mut() {
            for s in ch.iter_mut() {
                *s *= g;
            }
        }
    }
}

skuiz_auv3::export_auv3!(Gain);

extern "C" {
    fn skuiz_auv3_init(app_group_dir: *const std::ffi::c_char) -> *mut c_void;
    fn skuiz_auv3_destroy(inst: *mut c_void);
    fn skuiz_auv3_activate(inst: *mut c_void, sample_rate: f64, max_frames: u32);
    fn skuiz_auv3_deactivate(inst: *mut c_void);
    fn skuiz_auv3_render(
        inst: *mut c_void,
        channels: *const *mut f32,
        channel_count: u32,
        frames: u32,
    );
    fn skuiz_auv3_get_param(inst: *mut c_void, id: u32) -> f64;
    fn skuiz_auv3_set_param(inst: *mut c_void, id: u32, value: f64);
    fn skuiz_auv3_save_state(inst: *mut c_void, buf: *mut u8, cap: u32) -> u32;
    fn skuiz_auv3_load_state(inst: *mut c_void, buf: *const u8, len: u32) -> bool;
}

/// The instance pointer is not Send, but the discipline the engine enforces
/// — render calls only on the audio thread, main-thread entry points only on
/// the main thread — is exactly what the harness below keeps.
#[derive(Clone, Copy)]
struct SendPtr(*mut c_void);
unsafe impl Send for SendPtr {}

#[test]
fn hammers_params_and_state_while_rendering() {
    unsafe {
        let inst = SendPtr(skuiz_auv3_init(std::ptr::null()));
        assert!(!inst.0.is_null());
        skuiz_auv3_activate(inst.0, 48_000.0, 512);

        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            // Audio thread: pull blocks as fast as they come.
            let audio = s.spawn(move || {
                let inst = inst; // capture the Send wrapper whole, not the raw field
                let mut left = [0.5f32; 64];
                let mut right = [0.5f32; 64];
                for _ in 0..4000 {
                    let ptrs: [*mut f32; 2] = [left.as_mut_ptr(), right.as_mut_ptr()];
                    skuiz_auv3_render(inst.0, ptrs.as_ptr(), 2, 64);
                }
            });

            // Bus thread: spam param frames the whole time.
            let bus_spam = s.spawn(|| {
                let node = skuiz_ipc::Bus::join("test.auv3hammer", |_| {});
                let mut i = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    let v = (i % 200) as f64 / 100.0;
                    node.send(format!("set_param 0 {v}").as_bytes());
                    i += 1;
                }
                drop(node);
            });

            // Main thread: params and state save/load, the way a host
            // interleaves them. State ops while running are a bounded
            // round-trip through the audio thread, and blocks are flowing,
            // so every save must succeed.
            let mut saved;
            for i in 0..200u32 {
                skuiz_auv3_set_param(inst.0, 0, (i % 200) as f64 / 100.0);
                let got = skuiz_auv3_get_param(inst.0, 0);
                assert!((0.0..=2.0).contains(&got), "read out of range: {got}");
                if i % 20 == 0 {
                    let size = skuiz_auv3_save_state(inst.0, std::ptr::null_mut(), 0);
                    assert!(size > 0, "state save failed while rendering");
                    saved = vec![0u8; size as usize];
                    assert_eq!(
                        skuiz_auv3_save_state(inst.0, saved.as_mut_ptr(), size),
                        size
                    );
                    assert!(skuiz_auv3_load_state(inst.0, saved.as_ptr(), size));
                }
            }

            stop.store(true, Ordering::Relaxed);
            // Bounded wait: a deadlock in the engine must fail the test, not
            // hang the suite.
            let deadline = Instant::now() + Duration::from_secs(10);
            while !audio.is_finished() {
                assert!(Instant::now() < deadline, "audio thread wedged");
                std::thread::sleep(Duration::from_millis(5));
            }
            audio.join().expect("audio thread panicked");
            bus_spam.join().expect("bus thread panicked");
        });

        // Drain anything still queued, then every read must be a real value.
        let mut left = [0.0f32; 64];
        let mut right = [0.0f32; 64];
        let ptrs: [*mut f32; 2] = [left.as_mut_ptr(), right.as_mut_ptr()];
        for _ in 0..4 {
            skuiz_auv3_render(inst.0, ptrs.as_ptr(), 2, 64);
        }
        let final_value = skuiz_auv3_get_param(inst.0, 0);
        assert!(
            final_value.is_finite() && (0.0..=2.0).contains(&final_value),
            "final value is not a real parameter value: {final_value}"
        );

        skuiz_auv3_deactivate(inst.0);
        skuiz_auv3_destroy(inst.0);
    }
}
