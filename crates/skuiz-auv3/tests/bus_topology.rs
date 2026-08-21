//! Host-level test for the declarative bus topology: a sidechain effect
//! driven through the generated C ABI. Verifies the declared buses are
//! exposed through `skuiz_auv3_audio_bus_count`/`skuiz_auv3_audio_bus_info`
//! and that an optional sidechain is visible to the DSP exactly when the
//! caller (standing in for the shim) connects it.

use skuiz_auv3::{SkuizAudioBusBuffers, SkuizAudioBusInfo};
use skuiz_core::bus::{validate_buses, BusId};
use skuiz_core::{
    AudioBusSpec, AudioInputs, AudioOutputs, ChannelLayout, MidiOut, ParamDef, PluginInfo,
    Processor,
};
use std::ffi::{c_void, CStr};

/// Main stereo in/out plus an optional mono sidechain. The DSP writes a
/// marker into the output so the test can tell which buses it saw:
/// sidechain sample + 100 when the sidechain is connected, main input
/// sample * 2 when only the main pair is, -1 when nothing is.
struct SidechainFx;

impl Default for SidechainFx {
    fn default() -> Self {
        Self
    }
}

const SIDECHAIN: &[AudioBusSpec] = &[
    AudioBusSpec::input("Main", ChannelLayout::Stereo),
    AudioBusSpec::input("Sidechain", ChannelLayout::Mono).optional(),
    AudioBusSpec::output("Main", ChannelLayout::Stereo),
];

impl Processor for SidechainFx {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "test.auv3-sidechain-fx",
            name: "sc",
            vendor: "t",
            version: "0",
            description: "",
        }
    }
    fn params() -> &'static [ParamDef] {
        &[]
    }
    fn audio_buses() -> &'static [AudioBusSpec] {
        SIDECHAIN
    }
    fn set_param(&mut self, _id: u32, _v: f64) {}
    fn get_param(&self, _id: u32) -> f64 {
        0.0
    }
    fn process(&mut self, inputs: &AudioInputs, outputs: &mut AudioOutputs, _midi: &mut MidiOut) {
        let side = inputs
            .get(BusId::input("Sidechain"))
            .and_then(|b| b.channel(0))
            .map(|c| c[0]);
        let main_in = inputs.main().and_then(|b| b.channel(0)).map(|c| c[0]);
        let Some(out) = outputs.main() else {
            return;
        };
        for ch in out.channels() {
            for s in ch.iter_mut() {
                *s = match (side, main_in) {
                    (Some(sc), _) => 100.0 + sc,
                    (None, Some(m)) => m * 2.0,
                    (None, None) => -1.0,
                };
            }
        }
    }
}

skuiz_auv3::export_auv3!(SidechainFx);

extern "C" {
    fn skuiz_auv3_init(app_group_dir: *const std::ffi::c_char) -> *mut c_void;
    fn skuiz_auv3_destroy(inst: *mut c_void);
    fn skuiz_auv3_activate(inst: *mut c_void, sample_rate: f64, max_frames: u32);
    fn skuiz_auv3_deactivate(inst: *mut c_void);
    fn skuiz_auv3_audio_bus_count(direction: u8) -> u32;
    fn skuiz_auv3_audio_bus_info(direction: u8, index: u32, out: *mut SkuizAudioBusInfo) -> bool;
    fn skuiz_auv3_render(
        inst: *mut c_void,
        inputs: *const SkuizAudioBusBuffers,
        outputs: *const SkuizAudioBusBuffers,
        frames: u32,
    );
}

#[test]
fn declared_buses_are_exposed() {
    assert!(validate_buses(SIDECHAIN).is_ok());
    unsafe {
        assert_eq!(skuiz_auv3_audio_bus_count(0), 2, "two input buses");
        assert_eq!(skuiz_auv3_audio_bus_count(1), 1, "one output bus");

        let mut info = SkuizAudioBusInfo {
            channel_count: 0,
            optional: 0,
            name: std::ptr::null(),
        };

        assert!(skuiz_auv3_audio_bus_info(0, 0, &mut info));
        assert_eq!(info.channel_count, 2);
        assert_eq!(info.optional, 0, "the main bus is never optional");
        assert_eq!(CStr::from_ptr(info.name).to_str().unwrap(), "Main");

        assert!(skuiz_auv3_audio_bus_info(0, 1, &mut info));
        assert_eq!(info.channel_count, 1);
        assert_eq!(info.optional, 1);
        assert_eq!(CStr::from_ptr(info.name).to_str().unwrap(), "Sidechain");
        let sidechain_name = info.name;

        assert!(skuiz_auv3_audio_bus_info(1, 0, &mut info));
        assert_eq!(info.channel_count, 2);
        assert_eq!(info.optional, 0);
        assert_eq!(CStr::from_ptr(info.name).to_str().unwrap(), "Main");

        assert!(
            !skuiz_auv3_audio_bus_info(0, 2, &mut info),
            "no third input"
        );
        assert!(
            !skuiz_auv3_audio_bus_info(1, 1, &mut info),
            "no second output"
        );
        assert!(!skuiz_auv3_audio_bus_info(0, 0, std::ptr::null_mut()));

        // Repeated calls hand back the same cached name pointer.
        let mut again = SkuizAudioBusInfo {
            channel_count: 0,
            optional: 0,
            name: std::ptr::null(),
        };
        assert!(skuiz_auv3_audio_bus_info(0, 1, &mut again));
        assert_eq!(sidechain_name, again.name);
    }
}

/// One stereo main buffer pair (input aliases output, as the shim arranges
/// by pulling the upstream audio into the output buffers); the sidechain
/// wiring varies per call.
struct Buffers {
    main_l: [f32; 64],
    main_r: [f32; 64],
    side: [f32; 64],
}

impl Buffers {
    fn new() -> Self {
        Self {
            main_l: [0.5; 64],
            main_r: [0.5; 64],
            side: [0.25; 64],
        }
    }
}

unsafe fn run(bufs: &mut Buffers, inst: *mut c_void, with_sidechain: bool) {
    let mut outputs = [SkuizAudioBusBuffers::empty(); 4];
    outputs[0].channels[0] = bufs.main_l.as_mut_ptr();
    outputs[0].channels[1] = bufs.main_r.as_mut_ptr();
    outputs[0].channel_count = 2;
    outputs[0].active = 1;

    let mut inputs = [SkuizAudioBusBuffers::empty(); 4];
    if with_sidechain {
        // Slot 0 is ignored — the main input aliases the main output in
        // Rust — but the sidechain slot is read only when marked active.
        inputs[1].channels[0] = bufs.side.as_mut_ptr();
        inputs[1].channel_count = 1;
        inputs[1].active = 1;
    }
    skuiz_auv3_render(inst, inputs.as_ptr(), outputs.as_ptr(), 64);
}

#[test]
fn connected_sidechain_reaches_the_dsp() {
    unsafe {
        let inst = skuiz_auv3_init(std::ptr::null());
        assert!(!inst.is_null());
        skuiz_auv3_activate(inst, 48_000.0, 512);

        let mut bufs = Buffers::new();
        run(&mut bufs, inst, true);
        assert_eq!(
            bufs.main_l[0], 100.25,
            "sidechain sample visible in the DSP"
        );

        let mut bufs = Buffers::new();
        run(&mut bufs, inst, false);
        assert_eq!(
            bufs.main_l[0], 1.0,
            "no sidechain: main input (0.5) doubled by the DSP"
        );
        assert_eq!(bufs.main_r[63], 1.0, "whole block processed");

        skuiz_auv3_deactivate(inst);
        skuiz_auv3_destroy(inst);
    }
}

#[test]
fn no_output_bus_still_processes() {
    unsafe {
        let inst = skuiz_auv3_init(std::ptr::null());
        assert!(!inst.is_null());
        skuiz_auv3_activate(inst, 48_000.0, 512);

        // All outputs inactive (the shim's zeroed array): the DSP must run
        // without crashing and observe nothing connected.
        let outputs = [SkuizAudioBusBuffers::empty(); 4];
        skuiz_auv3_render(inst, std::ptr::null(), outputs.as_ptr(), 64);

        skuiz_auv3_deactivate(inst);
        skuiz_auv3_destroy(inst);
    }
}
