//! End-to-end IPC sync through the CLAP vtable: two plugin instances join
//! the bus on init; a frame sent by a third node must show up in both
//! instances' parameter values after their next process() call.

use clap_sys::ext::params::{clap_plugin_params, CLAP_EXT_PARAMS};
use clap_sys::plugin::clap_plugin;
use clap_sys::process::clap_process;
use skuiz_clap::ClapDescriptor;
use skuiz_core::{ParamDef, PluginInfo, Processor};
use std::ptr::{null, null_mut};
use std::time::Duration;

struct Gain(f64);

impl Default for Gain {
    fn default() -> Self {
        Self(1.0)
    }
}

impl Processor for Gain {
    fn info() -> PluginInfo {
        // Distinct id => distinct bus socket; keeps other tests out.
        PluginInfo {
            id: "test.ipcgain",
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

unsafe fn run_empty_process(plugin: *const clap_plugin) {
    let p = clap_process {
        steady_time: 0,
        frames_count: 0,
        transport: null(),
        audio_inputs: null(),
        audio_outputs: null_mut(),
        audio_inputs_count: 0,
        audio_outputs_count: 0,
        in_events: null(),
        out_events: null(),
    };
    ((*plugin).process.unwrap())(plugin, &p);
}

unsafe fn param_value(plugin: *const clap_plugin) -> f64 {
    let ext = ((*plugin).get_extension.unwrap())(plugin, CLAP_EXT_PARAMS.as_ptr());
    let params = &*(ext as *const clap_plugin_params);
    let mut v = f64::NAN;
    assert!((params.get_value.unwrap())(plugin, 0, &mut v));
    v
}

/// Wait for `cond`, polling rather than sleeping a fixed time: the whole
/// test suite runs in parallel, so any fixed delay is either flaky under
/// load or needlessly slow.
fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn bus_frame_reaches_all_instances() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<Gain>()));
        let a = skuiz_clap::instantiate::<Gain>(&desc.raw, null());
        let b = skuiz_clap::instantiate::<Gain>(&desc.raw, null());
        assert!(((*a).init.unwrap())(a));
        assert!(((*b).init.unwrap())(b));

        // A third node broadcasts a param change once everyone has joined.
        // No readiness wait is needed: the poll loop below re-sends the
        // frame every 20 ms, so sends lost while the bus is still forming
        // are simply retried.
        let outsider = skuiz_ipc::Bus::join("test.ipcgain", |_| {});

        // Values land on the next audio block, so keep pumping blocks while
        // waiting rather than assuming one is enough.
        wait_until("both instances to receive the IPC frame", || {
            outsider.send(b"set_param 0 0.3");
            run_empty_process(a);
            run_empty_process(b);
            param_value(a) == 0.3 && param_value(b) == 0.3
        });

        ((*a).destroy.unwrap())(a);
        ((*b).destroy.unwrap())(b);
        drop(outsider);
    }
}
