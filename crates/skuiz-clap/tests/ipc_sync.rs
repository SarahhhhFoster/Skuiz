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

struct Gain {
    gain: f64,
    local: f64,
}

impl Default for Gain {
    fn default() -> Self {
        Self {
            gain: 1.0,
            local: 0.5,
        }
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
        &[
            ParamDef {
                id: 0,
                name: "Gain",
                min: 0.0,
                max: 1.0,
                default: 1.0,
                choices: &[],
                shared: true,
            },
            ParamDef {
                id: 1,
                name: "Local",
                min: 0.0,
                max: 1.0,
                default: 0.5,
                choices: &[],
                shared: false,
            },
        ]
    }
    fn set_param(&mut self, id: u32, v: f64) {
        match id {
            0 => self.gain = v,
            1 => self.local = v,
            _ => {}
        }
    }
    fn get_param(&self, id: u32) -> f64 {
        match id {
            0 => self.gain,
            1 => self.local,
            _ => 0.0,
        }
    }
    fn process(
        &mut self,
        _inputs: &skuiz_core::AudioInputs,
        _outputs: &mut skuiz_core::AudioOutputs,
        _midi: &mut skuiz_core::MidiOut,
    ) {
    }
}

/// Same processor on its own bus scope: the convergence test must not share
/// a bus with the other two tests, whose repeated legacy frames would
/// legitimately apply to its instances and flap the asserted value.
#[derive(Default)]
struct ConvGain(Gain);

impl Processor for ConvGain {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "test.ipcconv",
            ..Gain::info()
        }
    }
    fn params() -> &'static [ParamDef] {
        Gain::params()
    }
    fn set_param(&mut self, id: u32, v: f64) {
        self.0.set_param(id, v);
    }
    fn get_param(&self, id: u32) -> f64 {
        self.0.get_param(id)
    }
    fn process(
        &mut self,
        inputs: &skuiz_core::AudioInputs,
        outputs: &mut skuiz_core::AudioOutputs,
        midi: &mut skuiz_core::MidiOut,
    ) {
        self.0.process(inputs, outputs, midi);
    }
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

/// A parameter declared `shared: false` must not sync: a bus frame naming
/// it is ignored, not applied (invariant 10).
#[test]
fn local_params_ignore_bus_frames() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<Gain>()));
        let a = skuiz_clap::instantiate::<Gain>(&desc.raw, null());
        assert!(((*a).init.unwrap())(a));

        let outsider = skuiz_ipc::Bus::join("test.ipcgain", |_| {});
        let ext = ((*a).get_extension.unwrap())(a, CLAP_EXT_PARAMS.as_ptr());
        let params = &*(ext as *const clap_plugin_params);
        let local_value = |plugin: *const clap_plugin| {
            let mut v = f64::NAN;
            assert!((params.get_value.unwrap())(plugin, 1, &mut v));
            v
        };

        // Re-send with blocks pumped, exactly like the positive test: if
        // the frame were applied, this loop would converge. It must not.
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            outsider.send(b"set_param 1 0.9");
            run_empty_process(a);
        }
        assert_eq!(
            local_value(a),
            0.5,
            "local parameter changed from a bus frame"
        );

        ((*a).destroy.unwrap())(a);
        drop(outsider);
    }
}

/// A late-joining instance must converge to the shared state the bus already
/// holds (invariant 9): it broadcasts `sync_request` on join and applies the
/// winning `sync_state` answer — without the answerer's untouched defaults
/// dragging anyone back.
#[test]
fn late_joiner_converges_to_shared_state() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<ConvGain>()));
        let a = skuiz_clap::instantiate::<ConvGain>(&desc.raw, null());
        assert!(((*a).init.unwrap())(a));

        // Establish state on A from a versioned frame, like a live edit on
        // some instance would. In-process delivery is synchronous, but pump
        // blocks so the engine drains however it was queued.
        let outsider = skuiz_ipc::Bus::join("test.ipcconv", |_| {});
        wait_until("instance A to take the versioned value", || {
            outsider.send(skuiz_core::protocol::set_param_versioned(0, 0.7, 5, 42).as_bytes());
            run_empty_process(a);
            param_value(a) == 0.7
        });

        // B joins late, still at the default. Its join-time sync_request must
        // pull A's value across with no further edits from anyone.
        let b = skuiz_clap::instantiate::<ConvGain>(&desc.raw, null());
        assert!(((*b).init.unwrap())(b));
        wait_until("the late joiner to converge", || {
            run_empty_process(a);
            run_empty_process(b);
            param_value(b) == 0.7
        });
        // B never saw an edit, so it omitted everything from its answer —
        // its untouched default must not reach back into A.
        run_empty_process(a);
        assert_eq!(
            param_value(a),
            0.7,
            "a joiner's untouched default displaced real state"
        );

        ((*a).destroy.unwrap())(a);
        ((*b).destroy.unwrap())(b);
        drop(outsider);
    }
}

/// A stale versioned frame must not clobber newer state (invariant 9).
#[test]
fn stale_versioned_frames_lose() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<ConvGain>()));
        let a = skuiz_clap::instantiate::<ConvGain>(&desc.raw, null());
        assert!(((*a).init.unwrap())(a));

        let outsider = skuiz_ipc::Bus::join("test.ipcconv", |_| {});
        wait_until("the newer version to land", || {
            outsider.send(skuiz_core::protocol::set_param_versioned(0, 0.7, 5, 42).as_bytes());
            run_empty_process(a);
            param_value(a) == 0.7
        });

        // Older seq, different origin: if applied, this would show as 0.1.
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            outsider.send(skuiz_core::protocol::set_param_versioned(0, 0.1, 3, 99).as_bytes());
            run_empty_process(a);
        }
        assert_eq!(param_value(a), 0.7, "a stale versioned frame was applied");

        ((*a).destroy.unwrap())(a);
        drop(outsider);
    }
}

/// Same processor on a third bus scope, so this test's frames cannot cross
/// into the other two tests running in parallel.
#[derive(Default)]
struct LoadGain(Gain);

impl Processor for LoadGain {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "test.ipcload",
            ..Gain::info()
        }
    }
    fn params() -> &'static [ParamDef] {
        Gain::params()
    }
    fn set_param(&mut self, id: u32, v: f64) {
        self.0.set_param(id, v);
    }
    fn get_param(&self, id: u32) -> f64 {
        self.0.get_param(id)
    }
    fn process(
        &mut self,
        inputs: &skuiz_core::AudioInputs,
        outputs: &mut skuiz_core::AudioOutputs,
        midi: &mut skuiz_core::MidiOut,
    ) {
        self.0.process(inputs, outputs, midi);
    }
}

// istream draining a Vec<u8> one byte at a time (worst case).
unsafe extern "C" fn state_read(
    stream: *const clap_sys::stream::clap_istream,
    buffer: *mut std::ffi::c_void,
    size: u64,
) -> i64 {
    let vec = &mut *((*stream).ctx as *mut Vec<u8>);
    if size == 0 || vec.is_empty() {
        return 0;
    }
    *(buffer as *mut u8) = vec.remove(0);
    1
}

/// A project-state load rewrites a shared parameter without a version, so
/// the instance must stop advertising it: a late joiner's sync_request gets
/// an answer that omits the parameter and the loaded value stays local
/// (invariant 10). A fresh shared edit afterwards is advertised again and
/// converges normally.
#[test]
fn project_load_does_not_leak_through_sync_state() {
    use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
    use clap_sys::stream::clap_istream;

    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<LoadGain>()));
        let a = skuiz_clap::instantiate::<LoadGain>(&desc.raw, null());
        assert!(((*a).init.unwrap())(a));

        // Establish bus state on A: a live edit would look like this.
        let outsider = skuiz_ipc::Bus::join("test.ipcload", |_| {});
        wait_until("instance A to take the versioned value", || {
            outsider.send(skuiz_core::protocol::set_param_versioned(0, 0.7, 5, 42).as_bytes());
            run_empty_process(a);
            param_value(a) == 0.7
        });

        // A loads project state setting the shared param to 0.2 — a local,
        // per-instance change (default format: magic + (id, value) pairs).
        // Stop first, as a host would: the convergence pumping above left
        // the instance in the processing state.
        ((*a).stop_processing.unwrap())(a);
        let mut payload = skuiz_core::STATE_MAGIC.to_vec();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&0.2f64.to_le_bytes());
        let ext = ((*a).get_extension.unwrap())(a, CLAP_EXT_STATE.as_ptr());
        let state = &*(ext as *const clap_plugin_state);
        let istream = clap_istream {
            ctx: &mut payload as *mut _ as *mut std::ffi::c_void,
            read: Some(state_read),
        };
        assert!((state.load.unwrap())(a, &istream), "state load failed");
        assert_eq!(param_value(a), 0.2);

        // B joins late. If A still advertised its stale version, B would
        // converge onto A's project value 0.2. It must keep its default.
        let b = skuiz_clap::instantiate::<LoadGain>(&desc.raw, null());
        assert!(((*b).init.unwrap())(b));
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            run_empty_process(a);
            run_empty_process(b);
        }
        assert_eq!(
            param_value(b),
            1.0,
            "project state leaked to a joiner under a stale version"
        );
        assert_eq!(param_value(a), 0.2, "the load itself was disturbed");

        // A fresh shared edit re-claims the parameter: advertised again,
        // and both instances converge.
        wait_until("a fresh edit to converge on both instances", || {
            outsider.send(skuiz_core::protocol::set_param_versioned(0, 0.4, 9, 42).as_bytes());
            run_empty_process(a);
            run_empty_process(b);
            param_value(a) == 0.4 && param_value(b) == 0.4
        });

        ((*a).destroy.unwrap())(a);
        ((*b).destroy.unwrap())(b);
        drop(outsider);
    }
}
