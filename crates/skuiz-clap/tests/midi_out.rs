//! A MIDI-emitting processor driven through the raw CLAP vtable: MIDI 1.0
//! events must arrive as `clap_event_midi`, wider UMP events as
//! `clap_event_midi2`, and the note port must advertise both dialects.

use clap_sys::events::{
    clap_event_header, clap_event_midi, clap_event_midi2, clap_output_events, CLAP_EVENT_MIDI,
    CLAP_EVENT_MIDI2,
};
use clap_sys::ext::note_ports::{
    clap_plugin_note_ports, CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_MIDI, CLAP_NOTE_DIALECT_MIDI2,
};
use clap_sys::process::clap_process;
use skuiz_clap::ClapDescriptor;
use skuiz_core::{MidiOut, ParamDef, PluginInfo, Processor};
use std::ffi::c_void;
use std::ptr::{null, null_mut};

/// Emits one MIDI 1.0 note on and one MIDI 2.0 note on per block.
#[derive(Default)]
struct TwoWorlds;

impl Processor for TwoWorlds {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "test.two-worlds",
            name: "t",
            vendor: "t",
            version: "0",
            description: "",
        }
    }
    fn params() -> &'static [ParamDef] {
        &[]
    }
    fn emits_midi() -> bool {
        true
    }
    fn set_param(&mut self, _id: u32, _v: f64) {}
    fn get_param(&self, _id: u32) -> f64 {
        0.0
    }
    fn process(
        &mut self,
        _inputs: &skuiz_core::AudioInputs,
        _outputs: &mut skuiz_core::AudioOutputs,
        midi: &mut MidiOut,
    ) {
        midi.push(3, skuiz_midi::note_on(0, 60, 100));
        midi.push(5, skuiz_midi::note_on2(1, 60, 0xF800));
    }
}

/// Collected host-visible events: `(type, frame, midi1 bytes or UMP words)`.
enum Seen {
    Midi1(u32, [u8; 3]),
    Ump(u32, [u32; 4]),
}

unsafe extern "C" fn collect(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    let sink = &mut *((*list).ctx as *mut Vec<Seen>);
    match (*event).type_ {
        CLAP_EVENT_MIDI => {
            let e = &*(event as *const clap_event_midi);
            sink.push(Seen::Midi1(e.header.time, e.data));
        }
        CLAP_EVENT_MIDI2 => {
            let e = &*(event as *const clap_event_midi2);
            sink.push(Seen::Ump(e.header.time, e.data));
        }
        _ => {}
    }
    true
}

#[test]
fn midi1_and_midi2_events_take_their_own_wire_format() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<TwoWorlds>()));
        let plugin = skuiz_clap::instantiate::<TwoWorlds>(&desc.raw, null());
        assert!(((*plugin).init.unwrap())(plugin));
        assert!(((*plugin).activate.unwrap())(plugin, 48_000.0, 1, 512));

        // The note port must tell the host both dialects are available.
        let ext = ((*plugin).get_extension.unwrap())(plugin, CLAP_EXT_NOTE_PORTS.as_ptr());
        let ports = &*(ext as *const clap_plugin_note_ports);
        let mut info = std::mem::zeroed();
        assert!((ports.get.unwrap())(plugin, 0, false, &mut info));
        assert_ne!(info.supported_dialects & CLAP_NOTE_DIALECT_MIDI, 0);
        assert_ne!(info.supported_dialects & CLAP_NOTE_DIALECT_MIDI2, 0);

        let mut sink: Vec<Seen> = Vec::new();
        let out_events = clap_output_events {
            ctx: &mut sink as *mut _ as *mut c_void,
            try_push: Some(collect),
        };
        let p = clap_process {
            steady_time: 0,
            frames_count: 64,
            transport: null(),
            audio_inputs: null(),
            audio_outputs: null_mut(),
            audio_inputs_count: 0,
            audio_outputs_count: 0,
            in_events: null(),
            out_events: &out_events,
        };
        ((*plugin).process.unwrap())(plugin, &p);

        assert_eq!(sink.len(), 2, "one MIDI 1.0 and one MIDI 2.0 event");
        match &sink[0] {
            Seen::Midi1(time, data) => {
                assert_eq!(*time, 3);
                assert_eq!(data, &[0x90, 60, 100], "MIDI 1.0 stays 3 bytes on the wire");
            }
            Seen::Ump(..) => panic!("first event should be MIDI 1.0"),
        }
        match &sink[1] {
            Seen::Ump(time, data) => {
                assert_eq!(*time, 5);
                assert_eq!(
                    &data[..2],
                    &[0x4091_3C00, 0xF800_0000],
                    "MIDI 2.0 goes out as UMP"
                );
            }
            Seen::Midi1(..) => panic!("second event should be a UMP midi2 event"),
        }

        ((*plugin).destroy.unwrap())(plugin);
    }
}
