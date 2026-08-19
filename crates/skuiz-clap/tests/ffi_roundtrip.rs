//! Exercises the adapter through its raw CLAP vtable, the way a host would:
//! instantiate → set a param via flush events → save state → load into a
//! second instance → read the param back.

use clap_sys::events::{
    clap_event_header, clap_event_param_value, clap_input_events, CLAP_CORE_EVENT_SPACE_ID,
    CLAP_EVENT_PARAM_VALUE,
};
use clap_sys::ext::params::{clap_plugin_params, CLAP_EXT_PARAMS};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::plugin::clap_plugin;
use clap_sys::stream::{clap_istream, clap_ostream};
use skuiz_clap::ClapDescriptor;
use skuiz_core::{ParamDef, PluginInfo, Processor};
use std::ffi::{c_char, c_void};
use std::ptr::null_mut;

struct Gain(f64);

impl Default for Gain {
    fn default() -> Self {
        Self(1.0)
    }
}

impl Processor for Gain {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "test.gain",
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
            max: 1.0,
            default: 1.0,
            choices: &[],
        }]
    }
    fn set_param(&mut self, _id: u32, v: f64) {
        self.0 = v;
    }
    fn get_param(&self, _id: u32) -> f64 {
        self.0
    }
    fn process(&mut self, _channels: &mut [&mut [f32]], _midi: &mut skuiz_core::MidiOut) {}
}

unsafe fn ext<T>(plugin: *const clap_plugin, id: &std::ffi::CStr) -> &'static T {
    let p = ((*plugin).get_extension.unwrap())(plugin, id.as_ptr());
    assert!(!p.is_null());
    &*(p as *const T)
}

// one-event input list
unsafe extern "C" fn ev_size(_: *const clap_input_events) -> u32 {
    1
}
unsafe extern "C" fn ev_get(
    list: *const clap_input_events,
    _index: u32,
) -> *const clap_event_header {
    (*list).ctx as *const clap_event_header
}

/// Writes the current gain into every sample, so a test can see where in
/// the block a parameter change took effect.
struct Fill(f64);

impl Default for Fill {
    fn default() -> Self {
        Self(1.0)
    }
}

impl Processor for Fill {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "test.fill",
            name: "f",
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
            max: 1.0,
            default: 1.0,
            choices: &[],
        }]
    }
    fn set_param(&mut self, _id: u32, v: f64) {
        self.0 = v;
    }
    fn get_param(&self, _id: u32) -> f64 {
        self.0
    }
    fn process(&mut self, channels: &mut [&mut [f32]], _midi: &mut skuiz_core::MidiOut) {
        for ch in channels.iter_mut() {
            ch.fill(self.0 as f32);
        }
    }
}

// ostream appending to a Vec<u8>, one byte at a time (worst case)
unsafe extern "C" fn sink_write(
    stream: *const clap_ostream,
    buffer: *const c_void,
    size: u64,
) -> i64 {
    if size == 0 {
        return 0;
    }
    let vec = &mut *((*stream).ctx as *mut Vec<u8>);
    vec.push(*(buffer as *const u8));
    1
}

// istream draining a Vec<u8>, one byte at a time (worst case)
unsafe extern "C" fn source_read(
    stream: *const clap_istream,
    buffer: *mut c_void,
    size: u64,
) -> i64 {
    let vec = &mut *((*stream).ctx as *mut Vec<u8>);
    if size == 0 || vec.is_empty() {
        return 0;
    }
    *(buffer as *mut u8) = vec.remove(0);
    1
}

#[test]
fn param_flush_then_state_roundtrip() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<Gain>()));
        let a = skuiz_clap::instantiate::<Gain>(&desc.raw, std::ptr::null());
        assert!(((*a).init.unwrap())(a));

        let params: &clap_plugin_params = ext(a, CLAP_EXT_PARAMS);
        let state: &clap_plugin_state = ext(a, CLAP_EXT_STATE);

        // host sets Gain = 0.0556 via a flush
        let mut ev = clap_event_param_value {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_param_value>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id: 0,
            cookie: null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value: 0.0556,
        };
        let in_events = clap_input_events {
            ctx: &mut ev as *mut _ as *mut c_void,
            size: Some(ev_size),
            get: Some(ev_get),
        };
        (params.flush.unwrap())(a, &in_events, null_mut());

        let mut got = 0.0f64;
        assert!((params.get_value.unwrap())(a, 0, &mut got));
        assert_eq!(got, 0.0556);

        // save from A
        let mut saved: Vec<u8> = Vec::new();
        let ostream = clap_ostream {
            ctx: &mut saved as *mut _ as *mut c_void,
            write: Some(sink_write),
        };
        assert!((state.save.unwrap())(a, &ostream));
        assert!(!saved.is_empty(), "state save wrote no bytes");

        // load into fresh B
        let b = skuiz_clap::instantiate::<Gain>(&desc.raw, std::ptr::null());
        assert!(((*b).init.unwrap())(b));
        let istream = clap_istream {
            ctx: &mut saved as *mut _ as *mut c_void,
            read: Some(source_read),
        };
        let state_b: &clap_plugin_state = ext(b, CLAP_EXT_STATE);
        assert!((state_b.load.unwrap())(b, &istream));

        let params_b: &clap_plugin_params = ext(b, CLAP_EXT_PARAMS);
        let mut got_b = 0.0f64;
        assert!((params_b.get_value.unwrap())(b, 0, &mut got_b));
        assert_eq!(got_b, 0.0556, "state did not round-trip through save/load");

        // text conversions while we're here
        let mut buf = [0 as c_char; 64];
        assert!((params.value_to_text.unwrap())(
            a,
            0,
            0.5,
            buf.as_mut_ptr(),
            64
        ));
        let mut parsed = 0.0f64;
        assert!((params.text_to_value.unwrap())(
            a,
            0,
            buf.as_ptr(),
            &mut parsed
        ));
        assert_eq!(parsed, 0.5);

        // unknown param ids must be refused, not formatted/parsed as numbers
        assert!(!(params.value_to_text.unwrap())(
            a,
            42,
            0.5,
            buf.as_mut_ptr(),
            64
        ));
        assert!(!(params.text_to_value.unwrap())(
            a,
            42,
            buf.as_ptr(),
            &mut parsed
        ));

        ((*a).destroy.unwrap())(a);
        ((*b).destroy.unwrap())(b);
    }
}

// two-event input list, delivered out of time order on purpose
unsafe extern "C" fn ev2_size(_: *const clap_input_events) -> u32 {
    2
}
unsafe extern "C" fn ev2_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    let events = &*((*list).ctx as *const [clap_event_param_value; 2]);
    &events[index as usize].header
}

fn param_event(time: u32, value: f64) -> clap_event_param_value {
    clap_event_param_value {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_param_value>() as u32,
            time,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_VALUE,
            flags: 0,
        },
        param_id: 0,
        cookie: null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value,
    }
}

/// A timed parameter event must take effect at its frame offset, not at the
/// top of the block: the adapter splits the block and renders segments.
#[test]
fn timed_param_events_split_the_block() {
    unsafe {
        use clap_sys::audio_buffer::clap_audio_buffer;
        use clap_sys::process::clap_process;

        let desc = Box::leak(Box::new(ClapDescriptor::new::<Fill>()));
        let plugin = skuiz_clap::instantiate::<Fill>(&desc.raw, std::ptr::null());
        assert!(((*plugin).init.unwrap())(plugin));

        // Out of order on the wire: 0.5 at frame 6, then 0.75 at frame 2.
        let events = [param_event(6, 0.5), param_event(2, 0.75)];
        let in_events = clap_input_events {
            ctx: &events as *const _ as *mut c_void,
            size: Some(ev2_size),
            get: Some(ev2_get),
        };

        let mut out_storage = [0.0f32; 8];
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
            frames_count: 8,
            transport: std::ptr::null(),
            audio_inputs: std::ptr::null(),
            audio_outputs: &mut out_buf,
            audio_inputs_count: 0,
            audio_outputs_count: 1,
            in_events: &in_events,
            out_events: std::ptr::null(),
        };
        ((*plugin).process.unwrap())(plugin, &p);

        assert_eq!(&out_storage[0..2], &[1.0; 2], "default before frame 2");
        assert_eq!(&out_storage[2..6], &[0.75; 4], "first event at frame 2");
        assert_eq!(&out_storage[6..8], &[0.5; 2], "second event at frame 6");

        // The final value must stick for the next block.
        let mut got = 0.0f64;
        let params: &clap_plugin_params = ext(plugin, CLAP_EXT_PARAMS);
        assert!((params.get_value.unwrap())(plugin, 0, &mut got));
        assert_eq!(got, 0.5);

        ((*plugin).destroy.unwrap())(plugin);
    }
}
