//! Host-level test for the declarative bus topology: a sidechain effect
//! driven through the raw CLAP vtable. Verifies the declared buses are
//! exposed through the audio-ports extension and that an optional sidechain
//! is visible to the DSP exactly when the host connects it.

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::ext::audio_ports::{
    clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS,
    CLAP_PORT_MONO, CLAP_PORT_STEREO,
};
use clap_sys::plugin::clap_plugin;
use clap_sys::process::clap_process;
use skuiz_clap::ClapDescriptor;
use skuiz_core::bus::{validate_buses, BusId};
use skuiz_core::{
    AudioBusSpec, AudioInputs, AudioOutputs, ChannelLayout, ParamDef, PluginInfo, Processor,
};
use std::ptr::{null, null_mut};

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
            id: "test.sidechain-fx",
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
    fn process(
        &mut self,
        inputs: &AudioInputs,
        outputs: &mut AudioOutputs,
        _midi: &mut skuiz_core::MidiOut,
    ) {
        let side = inputs
            .get(BusId::from_name("Sidechain"))
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

unsafe fn ports(plugin: *const clap_plugin) -> &'static clap_plugin_audio_ports {
    let p = ((*plugin).get_extension.unwrap())(plugin, CLAP_EXT_AUDIO_PORTS.as_ptr());
    assert!(!p.is_null());
    &*(p as *const clap_plugin_audio_ports)
}

unsafe fn port_info(
    plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
) -> Option<clap_audio_port_info> {
    let ports = ports(plugin);
    let mut info: clap_audio_port_info = std::mem::zeroed();
    if (ports.get.unwrap())(plugin, index, is_input, &mut info) {
        Some(info)
    } else {
        None
    }
}

fn name_of(info: &clap_audio_port_info) -> String {
    let end = info.name.iter().position(|&c| c == 0).unwrap_or(0);
    let bytes: Vec<u8> = info.name[..end].iter().map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn declared_buses_are_exposed() {
    assert!(validate_buses(SIDECHAIN).is_ok());
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<SidechainFx>()));
        let plugin = skuiz_clap::instantiate::<SidechainFx>(&desc.raw, null());
        assert!(((*plugin).init.unwrap())(plugin));

        let ports = ports(plugin);
        assert_eq!((ports.count.unwrap())(plugin, true), 2, "two input ports");
        assert_eq!((ports.count.unwrap())(plugin, false), 1, "one output port");

        let main_in = port_info(plugin, 0, true).expect("main input");
        assert_eq!(main_in.id, BusId::from_name("Main").0);
        assert_ne!(main_in.flags & CLAP_AUDIO_PORT_IS_MAIN, 0);
        assert_eq!(main_in.channel_count, 2);
        assert_eq!(main_in.port_type, CLAP_PORT_STEREO.as_ptr());
        assert_eq!(name_of(&main_in), "Main");
        // In-place pair names the matching main output.
        assert_eq!(main_in.in_place_pair, BusId::from_name("Main").0);

        let side = port_info(plugin, 1, true).expect("sidechain input");
        assert_eq!(side.id, BusId::from_name("Sidechain").0);
        assert_eq!(side.flags & CLAP_AUDIO_PORT_IS_MAIN, 0);
        assert_eq!(side.channel_count, 1);
        assert_eq!(side.port_type, CLAP_PORT_MONO.as_ptr());
        assert_eq!(name_of(&side), "Sidechain");

        let main_out = port_info(plugin, 0, false).expect("main output");
        assert_ne!(main_out.flags & CLAP_AUDIO_PORT_IS_MAIN, 0);
        assert_eq!(main_out.channel_count, 2);

        assert!(port_info(plugin, 2, true).is_none(), "no third input port");
        assert!(
            port_info(plugin, 1, false).is_none(),
            "no second output port"
        );

        ((*plugin).destroy.unwrap())(plugin);
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

unsafe fn run(bufs: &mut Buffers, plugin: *const clap_plugin, with_sidechain: bool) {
    let mut out_ptrs = [bufs.out_l.as_mut_ptr(), bufs.out_r.as_mut_ptr()];
    let mut out_buf = clap_audio_buffer {
        data32: out_ptrs.as_mut_ptr(),
        data64: null_mut(),
        channel_count: 2,
        latency: 0,
        constant_mask: 0,
    };
    let mut in_main_ptrs = [bufs.in_l.as_mut_ptr(), bufs.in_r.as_mut_ptr()];
    let in_main = clap_audio_buffer {
        data32: in_main_ptrs.as_mut_ptr(),
        data64: null_mut(),
        channel_count: 2,
        latency: 0,
        constant_mask: 0,
    };
    let mut side_ptrs = [bufs.side.as_mut_ptr()];
    let in_side = clap_audio_buffer {
        data32: side_ptrs.as_mut_ptr(),
        data64: null_mut(),
        channel_count: 1,
        latency: 0,
        constant_mask: 0,
    };
    // `audio_inputs` is an array of buffers by value; the sidechain slot is
    // present but unreferenced when the count leaves it out.
    let ins = [in_main, in_side];
    let p = clap_process {
        steady_time: 0,
        frames_count: 64,
        transport: null(),
        audio_inputs: ins.as_ptr(),
        audio_outputs: &mut out_buf,
        audio_inputs_count: if with_sidechain { 2 } else { 1 },
        audio_outputs_count: 1,
        in_events: null(),
        out_events: null(),
    };
    ((*plugin).process.unwrap())(plugin, &p);
}

#[test]
fn connected_sidechain_reaches_the_dsp() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<SidechainFx>()));
        let plugin = skuiz_clap::instantiate::<SidechainFx>(&desc.raw, null());
        assert!(((*plugin).init.unwrap())(plugin));
        assert!(((*plugin).activate.unwrap())(plugin, 48_000.0, 1, 512));

        let mut bufs = Buffers::new();
        run(&mut bufs, plugin, true);
        assert_eq!(bufs.out_l[0], 100.25, "sidechain sample visible in the DSP");

        run(&mut bufs, plugin, false);
        assert_eq!(
            bufs.out_l[0], 1.0,
            "no sidechain: main input (0.5) doubled by the DSP"
        );
        assert_eq!(bufs.out_r[63], 1.0, "whole block processed");

        ((*plugin).destroy.unwrap())(plugin);
    }
}
