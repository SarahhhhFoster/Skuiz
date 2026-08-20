//! `reset` must clear DSP state (and only DSP state) between blocks: a
//! counter the processor advances per block is observable in the rendered
//! audio, so the test can see the reset land without touching internals.

use skuiz_core::{MidiOut, ParamDef, PluginInfo, Processor};
use std::ffi::c_void;

struct Counter {
    gain: f64,
    blocks: u64,
}

impl Default for Counter {
    fn default() -> Self {
        Self {
            gain: 1.0,
            blocks: 0,
        }
    }
}

impl Processor for Counter {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "test.auv3reset",
            name: "c",
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
        self.gain = v;
    }
    fn get_param(&self, _id: u32) -> f64 {
        self.gain
    }
    fn reset(&mut self) {
        self.blocks = 0;
    }
    fn process(&mut self, channels: &mut [&mut [f32]], _midi: &mut MidiOut) {
        self.blocks += 1;
        // Frame 0 of channel 0 reports how many blocks have run since the
        // last reset; the rest is ordinary gain processing.
        for ch in channels.iter_mut() {
            for s in ch.iter_mut() {
                *s *= self.gain as f32;
            }
        }
        if let Some(first) = channels.first_mut().and_then(|c| c.first_mut()) {
            *first = self.blocks as f32;
        }
    }
}

skuiz_auv3::export_auv3!(Counter);

extern "C" {
    fn skuiz_auv3_init(app_group_dir: *const std::ffi::c_char) -> *mut c_void;
    fn skuiz_auv3_destroy(inst: *mut c_void);
    fn skuiz_auv3_activate(inst: *mut c_void, sample_rate: f64, max_frames: u32);
    fn skuiz_auv3_render(
        inst: *mut c_void,
        channels: *const *mut f32,
        channel_count: u32,
        frames: u32,
    );
    fn skuiz_auv3_reset(inst: *mut c_void);
    fn skuiz_auv3_get_param(inst: *mut c_void, id: u32) -> f64;
    fn skuiz_auv3_set_param(inst: *mut c_void, id: u32, value: f64);
}

#[test]
fn reset_clears_dsp_state_but_not_parameters() {
    unsafe {
        let inst = skuiz_auv3_init(std::ptr::null());
        assert!(!inst.is_null());
        skuiz_auv3_activate(inst, 48_000.0, 512);
        skuiz_auv3_set_param(inst, 0, 1.5);

        let mut left = [1.0f32; 32];
        let render = |left: &mut [f32; 32]| {
            left.fill(1.0); // fresh input each block, or the gain compounds
            let ptrs: [*mut f32; 1] = [left.as_mut_ptr()];
            skuiz_auv3_render(inst, ptrs.as_ptr(), 1, 32);
        };

        render(&mut left);
        assert_eq!(left[0], 1.0, "first block should report block 1");
        render(&mut left);
        assert_eq!(left[0], 2.0);

        skuiz_auv3_reset(inst);
        render(&mut left);
        assert_eq!(left[0], 1.0, "reset did not clear DSP state");
        assert_eq!(left[1], 1.5, "reset must not touch parameter values");
        assert_eq!(skuiz_auv3_get_param(inst, 0), 1.5);

        skuiz_auv3_destroy(inst);
    }
}
