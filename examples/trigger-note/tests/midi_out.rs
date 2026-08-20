//! Drives the example through the raw CLAP vtable and checks that the C
//! envelope follower's note events actually reach the host's output queue.

use skuiz_clap::clap_sys::audio_buffer::clap_audio_buffer;
use skuiz_clap::clap_sys::events::{
    clap_event_header, clap_event_midi, clap_output_events, CLAP_EVENT_MIDI,
};
use skuiz_clap::clap_sys::ext::note_ports::{clap_plugin_note_ports, CLAP_EXT_NOTE_PORTS};
use skuiz_clap::clap_sys::ext::params::{clap_plugin_params, CLAP_EXT_PARAMS};
use skuiz_clap::clap_sys::plugin::clap_plugin;
use skuiz_clap::clap_sys::process::clap_process;
use skuiz_clap::ClapDescriptor;
use skuiz_core::Processor;
use std::ffi::{c_char, c_void};
use std::ptr::{null, null_mut};
use trigger_note::TriggerNote;

/// Collects everything the plugin pushes, so tests can assert on real output.
unsafe extern "C" fn collect(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    let sink = &mut *((*list).ctx as *mut Vec<clap_event_midi>);
    if (*event).type_ == CLAP_EVENT_MIDI {
        sink.push(*(event as *const clap_event_midi));
    }
    true
}

/// Run one block of `samples` through the plugin, returning its MIDI output.
unsafe fn process_block(plugin: *const clap_plugin, samples: &mut [f32]) -> Vec<clap_event_midi> {
    let mut sink: Vec<clap_event_midi> = Vec::new();
    let out_events = clap_output_events {
        ctx: &mut sink as *mut _ as *mut c_void,
        try_push: Some(collect),
    };

    // Feed the signal as a real host would: via the audio input port, with a
    // separate output buffer for the adapter to copy into.
    let mut in_ptr = samples.as_mut_ptr();
    let in_buf = clap_audio_buffer {
        data32: &mut in_ptr,
        data64: null_mut(),
        channel_count: 1,
        latency: 0,
        constant_mask: 0,
    };
    let mut out_storage = vec![0.0f32; samples.len()];
    let mut out_ptr = out_storage.as_mut_ptr();
    let mut out_buf = clap_audio_buffer {
        data32: &mut out_ptr,
        data64: null_mut(),
        channel_count: 1,
        latency: 0,
        constant_mask: 0,
    };
    let p = clap_process {
        steady_time: 0,
        frames_count: samples.len() as u32,
        transport: null(),
        audio_inputs: &in_buf,
        audio_outputs: &mut out_buf,
        audio_inputs_count: 1,
        audio_outputs_count: 1,
        in_events: null(),
        out_events: &out_events,
    };
    ((*plugin).process.unwrap())(plugin, &p);
    sink
}

#[test]
fn c_dsp_emits_note_on_threshold_crossing() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<TriggerNote>()));
        let plugin = skuiz_clap::instantiate::<TriggerNote>(&desc.raw, null());
        assert!(((*plugin).init.unwrap())(plugin));
        assert!(((*plugin).activate.unwrap())(plugin, 48_000.0, 1, 512));

        // Silence must not trigger anything.
        let quiet = process_block(plugin, &mut [0.0f32; 512]);
        assert!(
            quiet.is_empty(),
            "silence should emit no MIDI, got {}",
            quiet.len()
        );

        // A loud block drives the C envelope past the default 0.1 threshold.
        let loud = process_block(plugin, &mut [0.8f32; 512]);
        assert_eq!(loud.len(), 1, "expected exactly one note-on");
        assert_eq!(loud[0].data, [0x90, 60, 100], "note on, ch 1, C3, vel 100");
        assert!(
            (loud[0].header.time as usize) < 512,
            "event time must fall inside the block"
        );

        // Still loud: the gate stays open, so no repeat note.
        let held = process_block(plugin, &mut [0.8f32; 512]);
        assert!(held.is_empty(), "held signal must not retrigger");

        // Signal falls away: the note is released once the envelope decays
        // below the close threshold. The 50 ms release means that takes
        // ~12 blocks at 512 frames / 48 kHz, so poll rather than assume.
        let mut released = Vec::new();
        for _ in 0..40 {
            released = process_block(plugin, &mut [0.0f32; 512]);
            if !released.is_empty() {
                break;
            }
        }
        assert_eq!(
            released.len(),
            1,
            "note-off never arrived as the signal decayed"
        );
        assert_eq!(released[0].data, [0x80, 60, 0]);

        // And it stays released: no stuck-note repeat.
        let after = process_block(plugin, &mut [0.0f32; 512]);
        assert!(after.is_empty(), "note-off must fire once, not every block");

        ((*plugin).destroy.unwrap())(plugin);
    }
}

#[test]
fn choice_params_drive_output_and_show_labels() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<TriggerNote>()));
        let plugin = skuiz_clap::instantiate::<TriggerNote>(&desc.raw, null());
        assert!(((*plugin).init.unwrap())(plugin));
        assert!(((*plugin).activate.unwrap())(plugin, 48_000.0, 1, 512));

        let ext = ((*plugin).get_extension.unwrap())(plugin, CLAP_EXT_PARAMS.as_ptr());
        let params = &*(ext as *const clap_plugin_params);

        // Choice params render as their labels, not as raw numbers.
        let mut buf = [0 as c_char; 64];
        assert!((params.value_to_text.unwrap())(
            plugin,
            1,
            3.0,
            buf.as_mut_ptr(),
            64
        ));
        let text = std::ffi::CStr::from_ptr(buf.as_ptr()).to_str().unwrap();
        assert_eq!(text, "D#3", "note dropdown should show a note name");

        // ...and accept that label back.
        let mut parsed = -1.0f64;
        assert!((params.text_to_value.unwrap())(
            plugin,
            1,
            buf.as_ptr(),
            &mut parsed
        ));
        assert_eq!(parsed, 3.0);

        // Selecting note index 3 and channel index 2 must change the wire bytes.
        let ext_note = ((*plugin).get_extension.unwrap())(plugin, CLAP_EXT_NOTE_PORTS.as_ptr());
        assert!(
            !ext_note.is_null(),
            "a MIDI-emitting plugin must expose note ports"
        );
        let note_ports = &*(ext_note as *const clap_plugin_note_ports);
        assert_eq!(
            (note_ports.count.unwrap())(plugin, false),
            1,
            "one MIDI output port"
        );
        assert_eq!(
            (note_ports.count.unwrap())(plugin, true),
            0,
            "no MIDI input port"
        );

        let mut proc_ = TriggerNote::default();
        proc_.set_param(1, 3.0); // D#3 => key 63
        proc_.set_param(2, 2.0); // channel index 2 => status nibble 2
        proc_.activate(48_000.0, 512);
        let mut midi = skuiz_core::MidiOut::with_capacity(8);
        let mut block = [0.8f32; 512];
        let mut chans: [&mut [f32]; 1] = [&mut block];
        proc_.process(&mut chans, &mut midi);
        assert_eq!(midi.events().len(), 1);
        assert_eq!(
            midi.events()[0].1.midi1_bytes(),
            Some([0x92, 63, 100]),
            "channel 3, D#3"
        );

        ((*plugin).destroy.unwrap())(plugin);
    }
}
