//! Calls the generated C ABI the way the Objective-C shim will, so the
//! boundary is exercised even though the Xcode target does not exist here.

use skuiz_auv3::{SkuizAudioBusBuffers, SkuizParamInfo};
use skuiz_core::{AudioInputs, AudioOutputs, MidiOut, ParamDef, PluginInfo, Processor};
use std::ffi::{c_void, CStr, CString};

struct Fixture {
    gain: f64,
    mode: f64,
}

impl Default for Fixture {
    fn default() -> Self {
        Self {
            gain: 1.0,
            mode: 0.0,
        }
    }
}

impl Processor for Fixture {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "test.auv3fixture",
            name: "Fixture",
            vendor: "Skuiz",
            version: "0",
            description: "",
        }
    }
    fn params() -> &'static [ParamDef] {
        // Both `shared: false`: these tests run in parallel in one process
        // and every instance joins the same plugin-id bus. Shared params
        // would let one test's live instance sync into another's (invariant
        // 9 working as designed — bus sync is covered by the ipc tests).
        &[
            ParamDef {
                id: 0,
                name: "Gain",
                min: 0.0,
                max: 2.0,
                default: 1.0,
                choices: &[],
                shared: false,
            },
            ParamDef {
                id: 1,
                name: "Mode",
                min: 0.0,
                max: 0.0,
                default: 0.0,
                choices: &["Off", "On", "Auto"],
                shared: false,
            },
        ]
    }
    fn emits_midi() -> bool {
        true
    }
    fn set_param(&mut self, id: u32, v: f64) {
        match id {
            0 => self.gain = v,
            1 => self.mode = v,
            _ => {}
        }
    }
    fn get_param(&self, id: u32) -> f64 {
        match id {
            0 => self.gain,
            1 => self.mode,
            _ => 0.0,
        }
    }
    fn process(&mut self, _inputs: &AudioInputs, outputs: &mut AudioOutputs, midi: &mut MidiOut) {
        let g = self.gain as f32;
        if let Some(out) = outputs.main() {
            for ch in out.channels() {
                for s in ch.iter_mut() {
                    *s *= g;
                }
            }
        }
        midi.push(7, skuiz_core::MidiEvent::from_midi1([0x90, 64, 100]));
    }
}

skuiz_auv3::export_auv3!(Fixture);

extern "C" {
    fn skuiz_auv3_init(app_group_dir: *const std::ffi::c_char) -> *mut c_void;
    fn skuiz_auv3_destroy(inst: *mut c_void);
    fn skuiz_auv3_activate(inst: *mut c_void, sample_rate: f64, max_frames: u32);
    fn skuiz_auv3_deactivate(inst: *mut c_void);
    fn skuiz_auv3_render(
        inst: *mut c_void,
        inputs: *const SkuizAudioBusBuffers,
        outputs: *const SkuizAudioBusBuffers,
        frames: u32,
    );
    fn skuiz_auv3_param_count() -> u32;
    fn skuiz_auv3_param_info(index: u32, out: *mut SkuizParamInfo) -> bool;
    fn skuiz_auv3_choice_label(param_id: u32, choice_index: u32) -> *const std::ffi::c_char;
    fn skuiz_auv3_get_param(inst: *mut c_void, id: u32) -> f64;
    fn skuiz_auv3_set_param(inst: *mut c_void, id: u32, value: f64);
    fn skuiz_auv3_set_param_from_render(inst: *mut c_void, id: u32, value: f64);
    fn skuiz_auv3_save_state(inst: *mut c_void, buf: *mut u8, cap: u32) -> u32;
    fn skuiz_auv3_load_state(inst: *mut c_void, buf: *const u8, len: u32) -> bool;
    fn skuiz_auv3_midi_count(inst: *mut c_void) -> u32;
    fn skuiz_auv3_midi_event(
        inst: *mut c_void,
        index: u32,
        frame: *mut u32,
        bytes3: *mut u8,
    ) -> bool;
}

#[test]
fn renders_audio_and_reports_midi() {
    unsafe {
        // An App Group container path, as the shim will supply on Apple platforms.
        let dir = std::env::temp_dir().join(format!("skuiz-auv3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let group = CString::new(dir.to_str().unwrap()).unwrap();

        let inst = skuiz_auv3_init(group.as_ptr());
        assert!(!inst.is_null());
        skuiz_auv3_activate(inst, 48_000.0, 512);

        skuiz_auv3_set_param(inst, 0, 0.5);
        assert_eq!(skuiz_auv3_get_param(inst, 0), 0.5);

        // The render-thread variant must also apply (it skips only the
        // broadcast), and must ignore unknown ids like the main one.
        skuiz_auv3_set_param_from_render(inst, 0, 0.75);
        assert_eq!(skuiz_auv3_get_param(inst, 0), 0.75);
        skuiz_auv3_set_param_from_render(inst, 999, 0.1);
        assert_eq!(skuiz_auv3_get_param(inst, 0), 0.75);
        skuiz_auv3_set_param(inst, 0, 0.5);

        let mut left = [1.0f32; 64];
        let mut right = [1.0f32; 64];
        // Default effect topology: main in aliases main out, so the buffers
        // below carry the input and receive the output, as the shim arranges.
        let mut outputs = [SkuizAudioBusBuffers::empty(); 4];
        outputs[0].channels[0] = left.as_mut_ptr();
        outputs[0].channels[1] = right.as_mut_ptr();
        outputs[0].channel_count = 2;
        outputs[0].active = 1;
        skuiz_auv3_render(inst, std::ptr::null(), outputs.as_ptr(), 64);

        assert!(
            left.iter().all(|s| (s - 0.5).abs() < 1e-6),
            "audio not processed"
        );
        assert!(right.iter().all(|s| (s - 0.5).abs() < 1e-6));

        assert_eq!(skuiz_auv3_midi_count(inst), 1);
        let mut frame = 0u32;
        let mut bytes = [0u8; 3];
        assert!(skuiz_auv3_midi_event(
            inst,
            0,
            &mut frame,
            bytes.as_mut_ptr()
        ));
        assert_eq!(frame, 7);
        assert_eq!(bytes, [0x90, 64, 100]);
        assert!(!skuiz_auv3_midi_event(
            inst,
            9,
            &mut frame,
            bytes.as_mut_ptr()
        ));

        skuiz_auv3_deactivate(inst);
        skuiz_auv3_destroy(inst);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn exposes_parameters_and_choice_labels() {
    unsafe {
        assert_eq!(skuiz_auv3_param_count(), 2);

        let mut info = SkuizParamInfo {
            id: 0,
            name: std::ptr::null(),
            min: 0.0,
            max: 0.0,
            default: 0.0,
            choice_count: 0,
        };
        assert!(skuiz_auv3_param_info(1, &mut info));
        assert_eq!(info.id, 1);
        assert_eq!(CStr::from_ptr(info.name).to_str().unwrap(), "Mode");
        // Choice params report the index range, not the unused min/max.
        assert_eq!((info.min, info.max), (0.0, 2.0));
        assert_eq!(info.choice_count, 3);

        let labels: Vec<_> = (0..3)
            .map(|i| {
                CStr::from_ptr(skuiz_auv3_choice_label(1, i))
                    .to_str()
                    .unwrap()
            })
            .collect();
        assert_eq!(labels, ["Off", "On", "Auto"]);
        assert!(
            skuiz_auv3_choice_label(1, 99).is_null(),
            "out of range must be null"
        );
        assert!(
            skuiz_auv3_choice_label(0, 0).is_null(),
            "continuous param has no labels"
        );

        // Repeated calls must hand back the same cached pointer rather than
        // leaking a fresh allocation each time the host rebuilds its tree.
        assert_eq!(skuiz_auv3_choice_label(1, 0), skuiz_auv3_choice_label(1, 0));
    }
}

#[test]
fn state_round_trips_with_size_query() {
    unsafe {
        let a = skuiz_auv3_init(std::ptr::null());
        skuiz_auv3_set_param(a, 0, 1.75);
        skuiz_auv3_set_param(a, 1, 2.0);

        // NULL buffer asks for the size, as the shim does before allocating.
        let size = skuiz_auv3_save_state(a, std::ptr::null_mut(), 0);
        assert!(size > 0, "state size query returned nothing");

        let mut buf = vec![0u8; size as usize];
        assert_eq!(skuiz_auv3_save_state(a, buf.as_mut_ptr(), size), size);

        let b = skuiz_auv3_init(std::ptr::null());
        assert!(skuiz_auv3_load_state(b, buf.as_ptr(), size));
        assert_eq!(skuiz_auv3_get_param(b, 0), 1.75);
        assert_eq!(skuiz_auv3_get_param(b, 1), 2.0);

        // Garbage must be rejected, not silently applied.
        assert!(!skuiz_auv3_load_state(b, [1u8, 2, 3].as_ptr(), 3));
        assert!(!skuiz_auv3_load_state(b, std::ptr::null(), 0));

        skuiz_auv3_destroy(a);
        skuiz_auv3_destroy(b);
    }
}
