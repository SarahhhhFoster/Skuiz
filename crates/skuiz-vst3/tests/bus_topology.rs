//! Host-level test for the declarative bus topology: a sidechain effect
//! driven through the COM vtable. Verifies the declared buses are exposed
//! through IComponent/IAudioProcessor and that an optional sidechain is
//! visible to the DSP exactly when the host activates and connects it.

#![allow(non_snake_case)]

use skuiz_core::bus::{validate_buses, BusId};
use skuiz_core::{
    AudioBusSpec, AudioInputs, AudioOutputs, ChannelLayout, MidiOut, ParamDef, PluginInfo,
    Processor,
};
use skuiz_vst3::vst3::Steinberg::Vst::*;
use skuiz_vst3::vst3::Steinberg::*;
use skuiz_vst3::vst3::{ComPtr, ComWrapper};
use skuiz_vst3::Vst3Plugin;

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
            id: "test.vst3sidechain",
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

/// One instance of the fixture, as COM pointers to its two bus interfaces.
unsafe fn instantiate() -> (ComPtr<IComponent>, ComPtr<IAudioProcessor>) {
    let plugin = ComWrapper::new(Vst3Plugin::<SidechainFx>::new());
    let component = plugin.to_com_ptr::<IComponent>().unwrap();
    let processor = plugin.to_com_ptr::<IAudioProcessor>().unwrap();
    assert_eq!(component.initialize(std::ptr::null_mut()), kResultOk);
    (component, processor)
}

const AUDIO: MediaType = MediaTypes_::kAudio as MediaType;
const INPUT: BusDirection = BusDirections_::kInput as BusDirection;
const OUTPUT: BusDirection = BusDirections_::kOutput as BusDirection;

fn bus_info(component: &ComPtr<IComponent>, dir: BusDirection, index: i32) -> Option<BusInfo> {
    let mut info: BusInfo = unsafe { std::mem::zeroed() };
    if unsafe { component.getBusInfo(AUDIO, dir, index, &mut info) } == kResultOk {
        Some(info)
    } else {
        None
    }
}

fn name_of(info: &BusInfo) -> String {
    let end = info
        .name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(info.name.len());
    String::from_utf16_lossy(&info.name[..end])
}

// The casts on the flag constants are platform-load-bearing: these enum
// constants are i32 on Windows and u32 on macOS, so clippy sees a
// same-type cast on one platform that the other requires.
#[allow(clippy::unnecessary_cast)]
#[test]
fn declared_buses_are_exposed() {
    assert!(validate_buses(SIDECHAIN).is_ok());
    unsafe {
        let (component, processor) = instantiate();

        assert_eq!(component.getBusCount(AUDIO, INPUT), 2, "two input buses");
        assert_eq!(component.getBusCount(AUDIO, OUTPUT), 1, "one output bus");

        let main_in = bus_info(&component, INPUT, 0).expect("main input");
        assert_eq!(main_in.channelCount, 2);
        assert_eq!(name_of(&main_in), "Main");
        assert_eq!(main_in.busType, BusTypes_::kMain as BusType);
        assert_ne!(
            main_in.flags & BusInfo_::BusFlags_::kDefaultActive as u32,
            0,
            "a non-optional bus is active by default"
        );

        let side = bus_info(&component, INPUT, 1).expect("sidechain input");
        assert_eq!(side.channelCount, 1);
        assert_eq!(name_of(&side), "Sidechain");
        assert_eq!(
            side.busType,
            BusTypes_::kAux as BusType,
            "sidechain is an aux bus"
        );
        assert_eq!(
            side.flags & BusInfo_::BusFlags_::kDefaultActive as u32,
            0,
            "an optional bus is not active by default"
        );

        let main_out = bus_info(&component, OUTPUT, 0).expect("main output");
        assert_eq!(main_out.channelCount, 2);
        assert_eq!(name_of(&main_out), "Main");
        assert_eq!(main_out.busType, BusTypes_::kMain as BusType);
        assert_ne!(
            main_out.flags & BusInfo_::BusFlags_::kDefaultActive as u32,
            0
        );

        assert!(
            bus_info(&component, INPUT, 2).is_none(),
            "no third input bus"
        );
        assert!(
            bus_info(&component, OUTPUT, 1).is_none(),
            "no second output bus"
        );

        // The declared layouts answer getBusArrangement.
        let mut arr: SpeakerArrangement = 0;
        assert_eq!(processor.getBusArrangement(INPUT, 0, &mut arr), kResultOk);
        assert_eq!(arr, SpeakerArr::kStereo);
        assert_eq!(processor.getBusArrangement(INPUT, 1, &mut arr), kResultOk);
        assert_eq!(arr, SpeakerArr::kMono);
        assert_eq!(processor.getBusArrangement(OUTPUT, 0, &mut arr), kResultOk);
        assert_eq!(arr, SpeakerArr::kStereo);
        assert_ne!(processor.getBusArrangement(INPUT, 2, &mut arr), kResultOk);

        assert_eq!(component.terminate(), kResultOk);
    }
}

/// The declared topology is fixed: the host must offer exactly the declared
/// bus counts and each bus's declared layout, with kEmpty allowed only for
/// a deactivated optional bus.
#[test]
fn arrangements_are_validated_against_the_declaration() {
    unsafe {
        let (_component, processor) = instantiate();

        let stereo_mono = [SpeakerArr::kStereo, SpeakerArr::kMono];
        let mut stereo = [SpeakerArr::kStereo];
        assert_eq!(
            processor.setBusArrangements(
                stereo_mono.as_ptr() as *mut SpeakerArrangement,
                2,
                stereo.as_mut_ptr(),
                1,
            ),
            kResultTrue,
            "the declared layouts must be accepted"
        );

        // A deactivated optional sidechain is kEmpty.
        let stereo_empty = [SpeakerArr::kStereo, SpeakerArr::kEmpty];
        assert_eq!(
            processor.setBusArrangements(
                stereo_empty.as_ptr() as *mut SpeakerArrangement,
                2,
                stereo.as_mut_ptr(),
                1,
            ),
            kResultTrue,
            "kEmpty stands in for a deactivated optional bus"
        );

        let mut rejects: Vec<(&str, Vec<SpeakerArrangement>, Vec<SpeakerArrangement>)> = vec![
            (
                "wrong sidechain layout",
                vec![SpeakerArr::kStereo, SpeakerArr::kStereo],
                vec![SpeakerArr::kStereo],
            ),
            (
                "mono main",
                vec![SpeakerArr::kMono, SpeakerArr::kMono],
                vec![SpeakerArr::kStereo],
            ),
            (
                "main cannot be deactivated",
                vec![SpeakerArr::kEmpty, SpeakerArr::kMono],
                vec![SpeakerArr::kStereo],
            ),
            (
                "sidechain missing",
                vec![SpeakerArr::kStereo],
                vec![SpeakerArr::kStereo],
            ),
            (
                "wrong output layout",
                vec![SpeakerArr::kStereo, SpeakerArr::kMono],
                vec![SpeakerArr::kMono],
            ),
        ];
        for (what, ins, outs) in rejects.iter_mut() {
            assert_eq!(
                processor.setBusArrangements(
                    ins.as_mut_ptr(),
                    ins.len() as i32,
                    outs.as_mut_ptr(),
                    outs.len() as i32,
                ),
                kResultFalse,
                "{what} must be rejected"
            );
        }
    }
}

/// One stereo main output buffer; input wiring varies per test.
struct Buffers {
    out_l: [f32; 64],
    out_r: [f32; 64],
    in_l: [f32; 64],
    in_r: [f32; 64],
    side: [f32; 64],
}

impl Buffers {
    fn new() -> Self {
        Self {
            out_l: [0.0; 64],
            out_r: [0.0; 64],
            in_l: [0.5; 64],
            in_r: [0.5; 64],
            side: [0.25; 64],
        }
    }
}

unsafe fn run(processor: &ComPtr<IAudioProcessor>, bufs: &mut Buffers, with_sidechain: bool) {
    let mut out_ptrs = [bufs.out_l.as_mut_ptr(), bufs.out_r.as_mut_ptr()];
    let mut out_bus = AudioBusBuffers {
        numChannels: 2,
        silenceFlags: 0,
        __field0: AudioBusBuffers__type0 {
            channelBuffers32: out_ptrs.as_mut_ptr(),
        },
    };
    let mut main_ptrs = [bufs.in_l.as_mut_ptr(), bufs.in_r.as_mut_ptr()];
    let mut side_ptrs = [bufs.side.as_mut_ptr()];
    // The sidechain slot is present but unreferenced when the count leaves
    // it out — a connected-but-unactivated host would do the same.
    let mut in_buses = [
        AudioBusBuffers {
            numChannels: 2,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: main_ptrs.as_mut_ptr(),
            },
        },
        AudioBusBuffers {
            numChannels: 1,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: side_ptrs.as_mut_ptr(),
            },
        },
    ];
    let mut data: ProcessData = std::mem::zeroed();
    data.numSamples = 64;
    data.numInputs = if with_sidechain { 2 } else { 1 };
    data.inputs = in_buses.as_mut_ptr();
    data.numOutputs = 1;
    data.outputs = &mut out_bus;
    data.symbolicSampleSize = SymbolicSampleSizes_::kSample32 as int32;
    assert_eq!(processor.process(&mut data), kResultOk);
}

/// activateBus controls whether the DSP sees the sidechain: active plus
/// buffers and the sidechain samples are there; deactivated and the bus is
/// inactive while the main pair still processes.
#[test]
fn activated_sidechain_reaches_the_dsp() {
    unsafe {
        let (component, processor) = instantiate();
        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kRealtime as int32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
            maxSamplesPerBlock: 512,
            sampleRate: 48_000.0,
        };
        assert_eq!(processor.setupProcessing(&mut setup), kResultOk);

        let mut bufs = Buffers::new();

        // Host activates the sidechain and connects buffers for it.
        assert_eq!(component.activateBus(AUDIO, INPUT, 1, 1), kResultOk);
        run(&processor, &mut bufs, true);
        assert_eq!(bufs.out_l[0], 100.25, "sidechain sample visible in the DSP");
        assert_eq!(bufs.out_r[63], 100.25, "whole block processed");

        // Host deactivates it; the same buffers now go unseen.
        assert_eq!(component.activateBus(AUDIO, INPUT, 1, 0), kResultOk);
        run(&processor, &mut bufs, true);
        assert_eq!(
            bufs.out_l[0], 1.0,
            "deactivated sidechain: main input (0.5) doubled by the DSP"
        );

        // Active but unconnected (no buffers in ProcessData) is inactive too.
        assert_eq!(component.activateBus(AUDIO, INPUT, 1, 1), kResultOk);
        run(&processor, &mut bufs, false);
        assert_eq!(bufs.out_l[0], 1.0, "unconnected sidechain must be inactive");

        // Activating a bus index beyond the declaration is an error.
        assert_ne!(component.activateBus(AUDIO, INPUT, 2, 1), kResultOk);

        assert_eq!(component.terminate(), kResultOk);
    }
}
