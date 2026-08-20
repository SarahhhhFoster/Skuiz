//! Runs the Objective-C `AUAudioUnit` shim for real.
//!
//! An `AUAudioUnit` subclass can be instantiated and rendered in-process,
//! with no extension bundle, host, or code signing, so the shim is executed
//! here rather than merely compiled. What this does *not* cover is the
//! packaging around it — Xcode target, entitlements, and the host actually
//! discovering the component.

#![cfg(target_os = "macos")]

use skuiz_core::{MidiOut, ParamDef, PluginInfo, Processor};

/// Parameter 0 is a plain gain multiplier, which is what the Objective-C
/// self test asserts against: set it to 0.25 and a buffer of ones must come
/// back quartered.
struct Fixture {
    gain: f64,
    mode: f64,
}

impl Default for Fixture {
    fn default() -> Self {
        Self {
            gain: 1.0,
            mode: 0.0,
        }
    }
}

impl Processor for Fixture {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "test.auv3shim",
            name: "Shim Fixture",
            vendor: "Skuiz",
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
                max: 2.0,
                default: 1.0,
                choices: &[],
            },
            ParamDef {
                id: 1,
                name: "Mode",
                min: 0.0,
                max: 0.0,
                default: 0.0,
                choices: &["Off", "On", "Auto"],
            },
        ]
    }
    fn emits_midi() -> bool {
        true
    }
    fn set_param(&mut self, id: u32, v: f64) {
        match id {
            0 => self.gain = v,
            1 => self.mode = v,
            _ => {}
        }
    }
    fn get_param(&self, id: u32) -> f64 {
        match id {
            0 => self.gain,
            1 => self.mode,
            _ => 0.0,
        }
    }
    fn process(&mut self, channels: &mut [&mut [f32]], midi: &mut MidiOut) {
        let g = self.gain as f32;
        for ch in channels.iter_mut() {
            for s in ch.iter_mut() {
                *s *= g;
            }
        }
        midi.push(0, skuiz_core::MidiEvent::from_midi1([0x90, 60, 100]));
    }
}

skuiz_auv3::export_auv3!(Fixture);

extern "C" {
    fn skuiz_auv3_selftest() -> std::ffi::c_int;
}

/// Maps the self test's exit codes back to the step that failed, so a
/// failure names the broken thing instead of a bare number.
fn describe(code: i32) -> &'static str {
    match code {
        0 => "success",
        1 => "AUAudioUnit could not be instantiated",
        2 => "parameter tree does not match the Rust parameter count",
        3 => "parameter tree is empty",
        4 => "setting a parameter did not reach Rust and back",
        5 => "allocateRenderResources failed",
        6 => "render returned an error with no input connected",
        7 => "no input should render silence, but did not",
        8 => "render returned an error with input connected",
        9 => "the processor did not apply gain to the pulled input",
        10 => "fullState did not produce a state blob",
        11 => "state did not survive a fullState save/load cycle",
        12 => "render returned an error with a MIDI output block installed",
        13 => "generated MIDI did not reach the host's MIDI output block",
        _ => "unknown failure",
    }
}

#[test]
fn objc_shim_renders_through_rust() {
    let code = unsafe { skuiz_auv3_selftest() };
    assert_eq!(
        code,
        0,
        "AUv3 shim self test failed at step {code}: {}",
        describe(code)
    );
}
