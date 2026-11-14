//! MIDI input through the raw CLAP vtable. An echo processor makes the
//! host-visible output prove exactly which bytes/words and frame offsets
//! reached `Processor::process`, including across block splits.

use clap_sys::events::{
    clap_event_header, clap_event_midi, clap_event_midi2, clap_event_note, clap_input_events,
    clap_output_events, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI, CLAP_EVENT_MIDI2,
    CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON,
};
use clap_sys::ext::note_ports::{clap_plugin_note_ports, CLAP_EXT_NOTE_PORTS};
use clap_sys::process::clap_process;
use skuiz_clap::ClapDescriptor;
use skuiz_core::{MidiIn, MidiOut, ParamDef, PluginInfo, Processor};
use std::ffi::c_void;
use std::ptr::{null, null_mut};

#[derive(Default)]
struct Echo;

impl Processor for Echo {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "test.midi-echo",
            name: "echo",
            vendor: "test",
            version: "0",
            description: "",
        }
    }
    fn params() -> &'static [ParamDef] {
        &[]
    }
    fn accepts_midi() -> bool {
        true
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
        notes: &MidiIn,
        midi: &mut MidiOut,
    ) {
        for (frame, event) in notes.events() {
            assert!(midi.push(frame, event));
        }
    }
}

#[derive(Debug, PartialEq)]
enum Seen {
    Midi1(u32, [u8; 3]),
    Ump(u32, [u32; 4]),
}

unsafe extern "C" fn input_size(list: *const clap_input_events) -> u32 {
    (*((*list).ctx as *const Vec<*const clap_event_header>)).len() as u32
}

unsafe extern "C" fn input_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    (&*((*list).ctx as *const Vec<*const clap_event_header>))[index as usize]
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

fn header(type_: u16, time: u32, size: usize) -> clap_event_header {
    clap_event_header {
        size: size as u32,
        time,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_,
        flags: 0,
    }
}

#[test]
fn note_midi1_and_midi2_inputs_reach_dsp_at_exact_offsets() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<Echo>()));
        let plugin = skuiz_clap::instantiate::<Echo>(&desc.raw, null());
        assert!(((*plugin).init.unwrap())(plugin));
        assert!(((*plugin).activate.unwrap())(plugin, 48_000.0, 1, 512));

        let ext = ((*plugin).get_extension.unwrap())(plugin, CLAP_EXT_NOTE_PORTS.as_ptr());
        let ports = &*(ext as *const clap_plugin_note_ports);
        assert_eq!((ports.count.unwrap())(plugin, true), 1);
        assert_eq!((ports.count.unwrap())(plugin, false), 1);

        // Deliberately unsorted: the adapter must establish block order.
        let off = clap_event_note {
            header: header(
                CLAP_EVENT_NOTE_OFF,
                63,
                std::mem::size_of::<clap_event_note>(),
            ),
            note_id: -1,
            port_index: 0,
            channel: 2,
            key: 67,
            velocity: 0.25,
        };
        let raw = clap_event_midi {
            header: header(CLAP_EVENT_MIDI, 7, std::mem::size_of::<clap_event_midi>()),
            port_index: 0,
            data: [0xB1, 74, 100],
        };
        let on = clap_event_note {
            header: header(
                CLAP_EVENT_NOTE_ON,
                20,
                std::mem::size_of::<clap_event_note>(),
            ),
            note_id: -1,
            port_index: 0,
            channel: 0,
            key: 60,
            velocity: 0.5,
        };
        let ump_words = [0x4092_3C00, 0xF800_0000, 0, 0];
        let ump = clap_event_midi2 {
            header: header(
                CLAP_EVENT_MIDI2,
                40,
                std::mem::size_of::<clap_event_midi2>(),
            ),
            port_index: 0,
            data: ump_words,
        };
        let input_ptrs = vec![&off.header, &raw.header, &on.header, &ump.header];
        let input = clap_input_events {
            ctx: &input_ptrs as *const _ as *mut c_void,
            size: Some(input_size),
            get: Some(input_get),
        };
        let mut sink: Vec<Seen> = Vec::new();
        let output = clap_output_events {
            ctx: &mut sink as *mut _ as *mut c_void,
            try_push: Some(collect),
        };
        let process = clap_process {
            steady_time: 0,
            frames_count: 64,
            transport: null(),
            audio_inputs: null(),
            audio_outputs: null_mut(),
            audio_inputs_count: 0,
            audio_outputs_count: 0,
            in_events: &input,
            out_events: &output,
        };
        ((*plugin).process.unwrap())(plugin, &process);

        assert_eq!(
            sink,
            vec![
                Seen::Midi1(7, [0xB1, 74, 100]),
                Seen::Midi1(20, [0x90, 60, 64]),
                Seen::Ump(40, ump_words),
                Seen::Midi1(63, [0x82, 67, 32]),
            ],
            "raw events survive and native notes convert without losing timing"
        );

        ((*plugin).destroy.unwrap())(plugin);
    }
}
