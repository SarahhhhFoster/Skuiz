//! pd-tremolo: a stereo tremolo whose DSP is an embedded Pure Data patch.
//!
//! The DSP is a Pd patch (`src/tremolo.pd`) running inside the plugin via
//! `PdEngine`: a stereo tremolo whose gain law is
//! `1 - depth/4 + (depth/4)·cos`, so full depth sweeps 0.5–1.0 and zero
//! depth is transparent. The Rate and Depth parameters reach the patch
//! through `[receive]` objects, which is the whole pattern for driving Pd
//! from plugin parameters. Because every `PdEngine` owns its own
//! `pdinstance`, two loaded instances modulate independently instead of
//! sharing one patch.
//!
//! The engine adds a constant `PdEngine::latency_frames` of delay (one
//! 64-frame Pd tick), reported to the host through `Processor::latency`.

#![cfg(feature = "libpd")]

use skuiz_core::{AudioInputs, AudioOutputs, MidiOut, ParamDef, PluginInfo, Processor};
use skuiz_dsp::PdEngine;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

const P_RATE: u32 = 0;
const P_DEPTH: u32 = 1;

/// The patch source, embedded so the cdylib is self-contained. libpd can
/// only load from a file path, so `activate` writes this to a temp file.
const PATCH: &str = include_str!("tremolo.pd");

pub struct PdTremolo {
    /// `None` until `activate` succeeds; a silent engine fails safe.
    pd: Option<PdEngine>,
    rate: f64,
    depth: f64,
    /// The temp file the running engine loaded, removed on deactivate.
    patch_file: Option<PathBuf>,
}

impl Default for PdTremolo {
    fn default() -> Self {
        Self {
            pd: None,
            rate: 5.0,
            depth: 1.0,
            patch_file: None,
        }
    }
}

impl Processor for PdTremolo {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "org.skuiz.pd-tremolo",
            name: "Pd Tremolo",
            vendor: "Skuiz",
            version: env!("CARGO_PKG_VERSION"),
            description: "Stereo tremolo whose DSP is an embedded Pure Data patch",
        }
    }

    fn params() -> &'static [ParamDef] {
        const PARAMS: &[ParamDef] = &[
            ParamDef {
                id: P_RATE,
                name: "Rate",
                min: 0.1,
                max: 10.0,
                default: 5.0,
                choices: &[],
                shared: true,
            },
            ParamDef {
                id: P_DEPTH,
                name: "Depth",
                min: 0.0,
                max: 1.0,
                default: 1.0,
                choices: &[],
                shared: true,
            },
        ];
        PARAMS
    }

    fn activate(&mut self, sample_rate: f64, _max_frames: u32) {
        self.deactivate();

        // One temp file per activation: instances must not race writing a
        // shared path while another instance's engine has it open.
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("skuiz-pd-tremolo-{}-{n}.pd", std::process::id()));

        // Engine and patch loading both allocate and take the global Pd
        // setup lock — this is the main-thread hook for exactly that.
        let Ok(mut pd) = PdEngine::new(sample_rate, 2) else {
            return;
        };
        if std::fs::write(&path, PATCH).is_err() || pd.open_patch(&path).is_err() {
            return;
        }
        // Push the current values: parameters set before activation (state
        // load) predate the patch, so its receivers never saw them.
        pd.send_float("rate", self.rate as f32);
        pd.send_float("depth", self.depth as f32);
        self.pd = Some(pd);
        self.patch_file = Some(path);
    }

    fn deactivate(&mut self) {
        self.pd = None;
        if let Some(path) = self.patch_file.take() {
            let _ = std::fs::remove_file(path);
        }
    }

    fn set_param(&mut self, id: u32, value: f64) {
        let (receiver, stored) = match id {
            P_RATE => {
                self.rate = value.clamp(0.1, 10.0);
                ("rate", self.rate)
            }
            P_DEPTH => {
                self.depth = value.clamp(0.0, 1.0);
                ("depth", self.depth)
            }
            _ => return,
        };
        if let Some(pd) = &mut self.pd {
            pd.send_float(receiver, stored as f32);
        }
    }

    fn get_param(&self, id: u32) -> f64 {
        match id {
            P_RATE => self.rate,
            P_DEPTH => self.depth,
            _ => 0.0,
        }
    }

    fn process(&mut self, _inputs: &AudioInputs, outputs: &mut AudioOutputs, _midi: &mut MidiOut) {
        // Pd still speaks the flat in-place channel array; the main output
        // bus (which the adapter already copied the input into) is that
        // array.
        let (Some(pd), Some(main)) = (self.pd.as_mut(), outputs.main()) else {
            return;
        };
        pd.process(main.channels());
    }

    fn latency(&self) -> u32 {
        // One Pd tick, whether or not the engine exists yet: latency must be
        // constant (see `Processor::latency`), so report it from the start.
        self.pd.as_ref().map_or(64, PdEngine::latency_frames)
    }

    fn editor_html() -> Option<&'static str> {
        Some(include_str!("editor.html"))
    }

    fn editor_size() -> (u32, u32) {
        (340, 130)
    }
}

skuiz_clap::export_clap!(PdTremolo);
