//! `reset` must clear DSP state (and only DSP state) between blocks: a
//! counter the processor advances per block is observable in the rendered
//! audio, so the test can see the reset land without touching internals.

use skuiz_auv3::SkuizAudioBusBuffers;
use skuiz_core::{AudioInputs, AudioOutputs, MidiOut, ParamDef, PluginInfo, Processor};
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
    fn process(&mut self, _inputs: &AudioInputs, outputs: &mut AudioOutputs, _midi: &mut MidiOut) {
        self.blocks += 1;
        // Frame 0 of channel 0 reports how many blocks have run since the
        // last reset; the rest is ordinary gain processing.
        let Some(out) = outputs.main() else { return };
        for ch in out.channels() {
            for s in ch.iter_mut() {
                *s *= self.gain as f32;
            }
        }
        if let Some(first) = out.channel_mut(0).and_then(|c| c.first_mut()) {
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
        inputs: *const SkuizAudioBusBuffers,
        outputs: *const SkuizAudioBusBuffers,
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
            let mut outputs = [SkuizAudioBusBuffers::empty(); 4];
            outputs[0].channels[0] = left.as_mut_ptr();
            outputs[0].channel_count = 1;
            outputs[0].active = 1;
            skuiz_auv3_render(inst, std::ptr::null(), outputs.as_ptr(), 32);
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
