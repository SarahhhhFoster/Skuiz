//! solid-synth: a SolidJS editor driving a Rust oscillator.
//!
//! The demo the other examples do not show: a real
//! reactive framework in the webview, whose state is the synth's state.
//! Solid signals hold frequency, waveform, level and cutoff; a
//! `createEffect` pushes each change down to this DSP, and sound follows.
//!
//! Solid and solid-knobs are vendored as prebuilt bundles (see
//! `src/vendor/`), so building this needs cargo and no JavaScript toolchain.

use skuiz_core::{MidiOut, ParamDef, PluginInfo, Processor};

const P_FREQ: u32 = 0;
const P_WAVE: u32 = 1;
const P_LEVEL: u32 = 2;
const P_CUTOFF: u32 = 3;

const WAVEFORMS: &[&str] = &["Sine", "Square", "Saw", "Triangle"];

pub struct SolidSynth {
    // Parameter targets, as set by the editor, host automation, or IPC.
    freq: f64,
    wave: f64,
    level: f64,
    cutoff: f64,

    sample_rate: f32,
    phase: f32,
    /// Smoothed values actually used per sample. Jumping straight to a new
    /// frequency or level on a parameter change is audible as a click, so
    /// both are ramped.
    freq_smoothed: f32,
    level_smoothed: f32,
    smoothing: f32,
    /// One-pole lowpass state.
    lowpass: f32,
}

impl Default for SolidSynth {
    fn default() -> Self {
        Self {
            freq: 220.0,
            wave: 0.0,
            level: 0.3,
            cutoff: 1.0,
            sample_rate: 48_000.0,
            phase: 0.0,
            freq_smoothed: 220.0,
            level_smoothed: 0.0,
            smoothing: 0.001,
            lowpass: 0.0,
        }
    }
}

impl SolidSynth {
    /// One cycle of the selected waveform, `phase` in 0..1.
    // ponytail: naive shapes, so the bright ones alias at high frequencies.
    // PolyBLEP if this ever needs to sound clean rather than demonstrate a
    // signal path.
    fn shape(&self, phase: f32) -> f32 {
        match self.wave.round() as u32 {
            1 => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            2 => 2.0 * phase - 1.0,
            3 => 4.0 * (phase - 0.5).abs() - 1.0,
            _ => (phase * std::f32::consts::TAU).sin(),
        }
    }
}

impl Processor for SolidSynth {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "org.skuiz.solid-synth",
            name: "Solid Synth",
            vendor: "Skuiz",
            version: env!("CARGO_PKG_VERSION"),
            description: "Oscillator driven by a SolidJS editor",
        }
    }

    fn params() -> &'static [ParamDef] {
        &[
            ParamDef {
                id: P_FREQ,
                name: "Frequency",
                min: 55.0,
                max: 1760.0,
                default: 220.0,
                choices: &[],
                shared: true,
            },
            ParamDef {
                id: P_WAVE,
                name: "Waveform",
                min: 0.0,
                max: 0.0,
                default: 0.0,
                choices: WAVEFORMS,
                shared: true,
            },
            ParamDef {
                id: P_LEVEL,
                name: "Level",
                min: 0.0,
                max: 1.0,
                default: 0.3,
                choices: &[],
                shared: true,
            },
            ParamDef {
                id: P_CUTOFF,
                name: "Cutoff",
                min: 0.0,
                max: 1.0,
                default: 1.0,
                choices: &[],
                shared: true,
            },
        ]
    }

    fn activate(&mut self, sample_rate: f64, _max_frames: u32) {
        self.sample_rate = sample_rate as f32;
        // ~20 ms ramp, expressed as a one-pole coefficient.
        self.smoothing = 1.0 - (-1.0 / (0.02 * self.sample_rate)).exp();
        self.phase = 0.0;
        self.lowpass = 0.0;
        self.freq_smoothed = self.freq as f32;
        // Start silent and ramp up, so loading the plugin is not a click.
        self.level_smoothed = 0.0;
    }

    fn set_param(&mut self, id: u32, value: f64) {
        match id {
            P_FREQ => self.freq = value.clamp(55.0, 1760.0),
            P_WAVE => self.wave = value.round().clamp(0.0, (WAVEFORMS.len() - 1) as f64),
            P_LEVEL => self.level = value.clamp(0.0, 1.0),
            P_CUTOFF => self.cutoff = value.clamp(0.0, 1.0),
            _ => {}
        }
    }

    fn get_param(&self, id: u32) -> f64 {
        match id {
            P_FREQ => self.freq,
            P_WAVE => self.wave,
            P_LEVEL => self.level,
            P_CUTOFF => self.cutoff,
            _ => 0.0,
        }
    }

    fn process(&mut self, channels: &mut [&mut [f32]], _midi: &mut MidiOut) {
        let Some(frames) = channels.first().map(|c| c.len()) else {
            return;
        };
        if frames == 0 {
            return;
        }

        // Cutoff maps exponentially from 20 Hz to ~18 kHz, which is how the
        // control sounds even rather than bunched at the top. Computed once
        // per block: the transcendentals are too costly per sample.
        let cutoff_hz = 20.0 * 900.0f32.powf(self.cutoff as f32);
        let filter_k = 1.0 - (-std::f32::consts::TAU * cutoff_hz / self.sample_rate).exp();
        let freq_target = self.freq as f32;
        let level_target = self.level as f32;

        for frame in 0..frames {
            self.freq_smoothed += self.smoothing * (freq_target - self.freq_smoothed);
            self.level_smoothed += self.smoothing * (level_target - self.level_smoothed);

            self.phase += self.freq_smoothed / self.sample_rate;
            if self.phase >= 1.0 {
                self.phase -= self.phase.floor();
            }

            let raw = self.shape(self.phase);
            self.lowpass += filter_k * (raw - self.lowpass);
            let sample = self.lowpass * self.level_smoothed;

            // The synth generates rather than processes, so it overwrites
            // whatever the host handed us.
            for ch in channels.iter_mut() {
                ch[frame] = sample;
            }
        }
    }

    fn editor_html() -> Option<&'static str> {
        // `concat!` keeps this a &'static str: the vendored Solid and
        // solid-knobs bundles and the page are separate files but one
        // compiled-in string.
        Some(concat!(
            include_str!("editor.head.html"),
            include_str!("vendor/solid.js"),
            include_str!("vendor/solid-knobs.js"),
            include_str!("editor.tail.html"),
        ))
    }

    fn editor_size() -> (u32, u32) {
        (420, 460)
    }
}

skuiz_clap::export_clap!(SolidSynth);

#[cfg(test)]
mod tests {
    use super::*;

    fn render(synth: &mut SolidSynth, frames: usize) -> Vec<f32> {
        let mut buf = vec![0.0f32; frames];
        let mut chans: [&mut [f32]; 1] = [&mut buf];
        let mut midi = MidiOut::with_capacity(4);
        synth.process(&mut chans, &mut midi);
        buf
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    #[test]
    fn produces_sound_and_level_controls_it() {
        let mut synth = SolidSynth::default();
        synth.activate(48_000.0, 512);

        // Let the level ramp settle before measuring.
        render(&mut synth, 48_000);
        let loud = render(&mut synth, 4_800);
        assert!(
            peak(&loud) > 0.1,
            "synth produced no sound: peak {}",
            peak(&loud)
        );

        synth.set_param(P_LEVEL, 0.0);
        render(&mut synth, 48_000);
        let quiet = render(&mut synth, 4_800);
        assert!(
            peak(&quiet) < 1e-3,
            "level 0 did not silence it: peak {}",
            peak(&quiet)
        );
    }

    #[test]
    fn parameter_changes_do_not_click() {
        // A jump straight to a new level would step the signal; smoothing
        // is what keeps a UI drag from crackling.
        let mut synth = SolidSynth::default();
        synth.activate(48_000.0, 512);
        render(&mut synth, 48_000);

        synth.set_param(P_LEVEL, 1.0);
        let after = render(&mut synth, 64);
        let max_step = after
            .windows(2)
            .fold(0.0f32, |m, w| m.max((w[1] - w[0]).abs()));
        assert!(
            max_step < 0.2,
            "level jump produced a step of {max_step}, which is an audible click"
        );
    }

    #[test]
    fn every_waveform_makes_a_distinct_signal() {
        let mut previous: Option<Vec<f32>> = None;
        for (index, name) in WAVEFORMS.iter().enumerate() {
            let mut synth = SolidSynth::default();
            synth.set_param(P_WAVE, index as f64);
            synth.set_param(P_LEVEL, 1.0);
            synth.activate(48_000.0, 512);
            render(&mut synth, 48_000);
            let output = render(&mut synth, 1_000);

            assert!(peak(&output) > 0.1, "{name} was silent");
            assert!(
                output.iter().all(|s| s.is_finite()),
                "{name} produced non-finite samples"
            );
            if let Some(prev) = &previous {
                let differs = prev.iter().zip(&output).any(|(a, b)| (a - b).abs() > 1e-3);
                assert!(
                    differs,
                    "{name} is indistinguishable from the previous waveform"
                );
            }
            previous = Some(output);
        }
    }

    #[test]
    fn cutoff_removes_high_frequencies() {
        // A closed filter on a saw must measurably reduce sample-to-sample
        // movement, which is the audible meaning of "darker".
        let mut bright = SolidSynth::default();
        bright.set_param(P_WAVE, 2.0);
        bright.set_param(P_LEVEL, 1.0);
        bright.set_param(P_FREQ, 440.0);
        bright.set_param(P_CUTOFF, 1.0);
        bright.activate(48_000.0, 512);
        render(&mut bright, 48_000);
        let open = render(&mut bright, 4_800);

        let mut dark = SolidSynth::default();
        dark.set_param(P_WAVE, 2.0);
        dark.set_param(P_LEVEL, 1.0);
        dark.set_param(P_FREQ, 440.0);
        dark.set_param(P_CUTOFF, 0.15);
        dark.activate(48_000.0, 512);
        render(&mut dark, 48_000);
        let closed = render(&mut dark, 4_800);

        let motion = |s: &[f32]| s.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f32>();
        assert!(
            motion(&closed) < motion(&open) * 0.5,
            "closing the filter did not darken the tone"
        );
    }
}
