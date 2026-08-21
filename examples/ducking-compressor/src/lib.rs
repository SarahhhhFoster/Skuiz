//! ducking-compressor: the Skuiz sidechain example.
//!
//! A rudimentary ducking compressor: a mono sidechain drives an envelope
//! follower, and the main stereo pair is turned down while the envelope
//! exceeds the threshold. The point of the example is the declarative bus
//! topology — the processor never touches host bus negotiation.

use skuiz_core::bus::BusId;
use skuiz_core::{
    AudioBusSpec, AudioInputs, AudioOutputs, ChannelLayout, MidiOut, ParamDef, PluginInfo,
    Processor,
};

const P_THRESHOLD: u32 = 0;
const P_DEPTH: u32 = 1;
const P_ATTACK: u32 = 2;
const P_RELEASE: u32 = 3;

/// Main stereo pair plus an optional mono sidechain. When the host leaves
/// the sidechain unconnected the bus reports inactive and the audio passes
/// through untouched.
const BUSES: &[AudioBusSpec] = &[
    AudioBusSpec::input("Main", ChannelLayout::Stereo),
    AudioBusSpec::input("Sidechain", ChannelLayout::Mono).optional(),
    AudioBusSpec::output("Main", ChannelLayout::Stereo),
];

pub struct DuckingCompressor {
    threshold: f64, // linear peak where ducking starts
    depth: f64,     // 0 = no ducking, 1 = full silence at maximum overage
    attack_ms: f64,
    release_ms: f64,
    env: f32,
    sample_rate: f64,
}

impl Default for DuckingCompressor {
    fn default() -> Self {
        Self {
            threshold: 0.3,
            depth: 0.8,
            attack_ms: 5.0,
            release_ms: 150.0,
            env: 0.0,
            sample_rate: 48_000.0,
        }
    }
}

/// One-pole coefficient for a time constant in milliseconds.
fn coef(ms: f64, sample_rate: f64) -> f32 {
    (1.0 - (-1.0 / (ms.max(0.01) * 0.001 * sample_rate)).exp()) as f32
}

impl Processor for DuckingCompressor {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "org.skuiz.ducking-compressor",
            name: "Ducking Compressor",
            vendor: "Skuiz",
            version: env!("CARGO_PKG_VERSION"),
            description: "Rudimentary sidechain ducking compressor",
        }
    }

    fn params() -> &'static [ParamDef] {
        &[
            ParamDef {
                id: P_THRESHOLD,
                name: "Threshold",
                min: 0.0,
                max: 1.0,
                default: 0.3,
                choices: &[],
                shared: false,
            },
            ParamDef {
                id: P_DEPTH,
                name: "Depth",
                min: 0.0,
                max: 1.0,
                default: 0.8,
                choices: &[],
                shared: false,
            },
            ParamDef {
                id: P_ATTACK,
                name: "Attack (ms)",
                min: 0.1,
                max: 200.0,
                default: 5.0,
                choices: &[],
                shared: false,
            },
            ParamDef {
                id: P_RELEASE,
                name: "Release (ms)",
                min: 1.0,
                max: 1000.0,
                default: 150.0,
                choices: &[],
                shared: false,
            },
        ]
    }

    fn audio_buses() -> &'static [AudioBusSpec] {
        BUSES
    }

    fn activate(&mut self, sample_rate: f64, _max_frames: u32) {
        self.sample_rate = sample_rate;
        self.env = 0.0;
    }

    fn reset(&mut self) {
        self.env = 0.0;
    }

    fn set_param(&mut self, id: u32, value: f64) {
        match id {
            P_THRESHOLD => self.threshold = value.clamp(0.0, 1.0),
            P_DEPTH => self.depth = value.clamp(0.0, 1.0),
            P_ATTACK => self.attack_ms = value.clamp(0.1, 200.0),
            P_RELEASE => self.release_ms = value.clamp(1.0, 1000.0),
            _ => {}
        }
    }

    fn get_param(&self, id: u32) -> f64 {
        match id {
            P_THRESHOLD => self.threshold,
            P_DEPTH => self.depth,
            P_ATTACK => self.attack_ms,
            P_RELEASE => self.release_ms,
            _ => 0.0,
        }
    }

    fn editor_html() -> Option<&'static str> {
        Some(include_str!("editor.html"))
    }

    fn editor_size() -> (u32, u32) {
        (320, 220)
    }

    fn process(&mut self, inputs: &AudioInputs, outputs: &mut AudioOutputs, _midi: &mut MidiOut) {
        // No sidechain connected: the bus reports inactive and yields no
        // channels, so the audio (already copied into the outputs by the
        // adapter) passes through untouched.
        let Some(side) = inputs
            .get(BusId::input("Sidechain"))
            .and_then(|b| b.channel(0))
        else {
            return;
        };
        let Some(main) = outputs.main() else {
            return;
        };

        let attack = coef(self.attack_ms, self.sample_rate);
        let release = coef(self.release_ms, self.sample_rate);
        let threshold = self.threshold as f32;
        let depth = self.depth as f32;
        let headroom = (1.0 - threshold).max(1e-6);
        let mut env = self.env;

        // Frame-outer loop: one envelope, one gain per frame applied to
        // every channel (mono-linked, so the stereo image never wobbles).
        let frames = main.frames().min(side.len());
        for i in 0..frames {
            let target = side[i].abs();
            let c = if target > env { attack } else { release };
            env += (target - env) * c;
            let over = (env - threshold).max(0.0);
            let gain = 1.0 - depth * (over / headroom).min(1.0);
            for ch in main.channels() {
                ch[i] *= gain;
            }
        }
        self.env = env;
    }
}

skuiz_clap::export_clap!(DuckingCompressor);

#[cfg(test)]
mod tests {
    use super::*;
    use skuiz_core::bus::TopologyScratch;
    use skuiz_core::BusDirection;

    const FRAMES: usize = 480;

    struct Buffers {
        main_in: [[f32; FRAMES]; 2],
        side: [f32; FRAMES],
        out: [[f32; FRAMES]; 2],
    }

    /// Run one block. `sidechain` = Some(level) connects the sidechain at a
    /// constant level; None leaves it inactive. The main input is a constant
    /// 1.0, pre-copied into the outputs the way the adapters do it.
    fn run(duck: &mut DuckingCompressor, sidechain: Option<f32>) -> [f32; FRAMES] {
        let mut bufs = Buffers {
            main_in: [[1.0; FRAMES]; 2],
            side: [sidechain.unwrap_or(0.0); FRAMES],
            out: [[1.0; FRAMES]; 2],
        };
        let mut scratch = TopologyScratch::new(DuckingCompressor::audio_buses());
        scratch.clear();
        scratch.set_active(BusDirection::Input, 0, true);
        scratch.set_active(BusDirection::Input, 1, sidechain.is_some());
        scratch.set_active(BusDirection::Output, 0, true);
        // SAFETY: `bufs` outlives the views; the view borrow ends before the
        // output is read back.
        unsafe {
            for c in 0..2 {
                scratch.set_channel(
                    BusDirection::Input,
                    0,
                    c,
                    bufs.main_in[c].as_mut_ptr(),
                    FRAMES,
                );
                scratch.set_channel(BusDirection::Output, 0, c, bufs.out[c].as_mut_ptr(), FRAMES);
            }
            scratch.set_channel(BusDirection::Input, 1, 0, bufs.side.as_mut_ptr(), FRAMES);
        }
        let (inputs, mut outputs) = scratch.views();
        let mut midi = MidiOut::with_capacity(4);
        duck.process(&inputs, &mut outputs, &mut midi);
        bufs.out[0]
    }

    #[test]
    fn loud_sidechain_ducks_the_main_signal() {
        let mut duck = DuckingCompressor::default();
        duck.activate(48_000.0, FRAMES as u32);

        // Let the envelope settle on a loud sidechain, then measure.
        for _ in 0..20 {
            run(&mut duck, Some(1.0));
        }
        let out = run(&mut duck, Some(1.0));
        // env ~= 1.0, over/headroom saturates at 1, so gain ~= 1 - depth.
        assert!(
            out.iter().all(|&s| (s - 0.2).abs() < 0.02),
            "expected gain ~0.2, got {}",
            out[0]
        );
    }

    #[test]
    fn quiet_sidechain_below_threshold_passes_audio() {
        let mut duck = DuckingCompressor::default();
        duck.activate(48_000.0, FRAMES as u32);

        for _ in 0..20 {
            run(&mut duck, Some(0.1));
        }
        let out = run(&mut duck, Some(0.1));
        assert!(
            out.iter().all(|&s| (s - 1.0).abs() < 1e-6),
            "below-threshold sidechain should not duck, got {}",
            out[0]
        );
    }

    #[test]
    fn no_sidechain_passes_audio_untouched() {
        let mut duck = DuckingCompressor::default();
        duck.activate(48_000.0, FRAMES as u32);
        let out = run(&mut duck, None);
        assert!(out.iter().all(|&s| s == 1.0));
    }

    #[test]
    fn gain_recovers_after_the_sidechain_drops() {
        let mut duck = DuckingCompressor::default();
        duck.activate(48_000.0, FRAMES as u32);

        for _ in 0..20 {
            run(&mut duck, Some(1.0));
        }
        // Sidechain gone: the release ramp should bring the gain back up.
        let mut last = 0.0f32;
        for _ in 0..20 {
            let out = run(&mut duck, Some(0.0));
            last = out[FRAMES - 1];
        }
        assert!(last > 0.95, "gain did not recover: {last}");
    }
}
