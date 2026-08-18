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

        ((*a).destroy.unwrap())(a);
        ((*b).destroy.unwrap())(b);
    }
}
