//! shared-gain: the Skuiz example plugin.
//!
//! A plain stereo gain, plus the webview UI and the IPC-shared parameter
//! this example exists to demonstrate.

use skuiz_core::{MidiOut, ParamDef, PluginInfo, Processor};

const P_GAIN: u32 = 0;

pub struct SharedGain {
    gain: f64,
}

impl Default for SharedGain {
    fn default() -> Self {
        Self { gain: 1.0 }
    }
}

impl Processor for SharedGain {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "org.skuiz.shared-gain",
            name: "Shared Gain",
            vendor: "Skuiz",
            version: env!("CARGO_PKG_VERSION"),
            description: "Gain with an IPC-shared parameter",
        }
    }

    fn params() -> &'static [ParamDef] {
        &[ParamDef {
            id: P_GAIN,
            name: "Gain",
            min: 0.0,
            max: 1.0,
            default: 1.0,
            choices: &[],
            shared: true,
        }]
    }

    fn set_param(&mut self, id: u32, value: f64) {
        if id == P_GAIN {
            self.gain = value.clamp(0.0, 1.0);
        }
    }

    fn get_param(&self, id: u32) -> f64 {
        if id == P_GAIN {
            self.gain
        } else {
            0.0
        }
    }

    fn editor_html() -> Option<&'static str> {
        Some(include_str!("editor.html"))
    }

    fn editor_size() -> (u32, u32) {
        (320, 120)
    }

    fn process(&mut self, channels: &mut [&mut [f32]], _midi: &mut MidiOut) {
        // No gain ramping: slider drags click. Deliberate for brevity —
        // see solid-synth for the smoothing pattern.
        let g = self.gain as f32;
        for ch in channels.iter_mut() {
            for s in ch.iter_mut() {
                *s *= g;
            }
        }
    }
}

skuiz_clap::export_clap!(SharedGain);

// Both entry points can live in one cdylib; each bundle format just wraps
// the same binary differently.
#[cfg(feature = "vst3")]
skuiz_vst3::export_vst3!(SharedGain);
