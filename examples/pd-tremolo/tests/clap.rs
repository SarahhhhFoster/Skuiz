//! Drives the example through the raw CLAP vtable and checks that the
//! embedded Pd patch actually processes audio, plus a processor-level test
//! that parameter changes reach the patch's `[receive]` objects.

use pd_tremolo::PdTremolo;
use skuiz_clap::clap_sys::audio_buffer::clap_audio_buffer;
use skuiz_clap::clap_sys::ext::latency::{clap_plugin_latency, CLAP_EXT_LATENCY};
use skuiz_clap::clap_sys::plugin::clap_plugin;
use skuiz_clap::clap_sys::process::clap_process;
use skuiz_clap::ClapDescriptor;
use skuiz_core::Processor;
use std::ptr::{null, null_mut};

/// Run one stereo block through the plugin, returning the output channels.
unsafe fn process_block(
    plugin: *const clap_plugin,
    left: &mut [f32],
    right: &mut [f32],
) -> (Vec<f32>, Vec<f32>) {
    let frames = left.len();
    assert_eq!(frames, right.len());

    let mut in_ptrs = [left.as_mut_ptr(), right.as_mut_ptr()];
    let in_buf = clap_audio_buffer {
        data32: in_ptrs.as_mut_ptr(),
        data64: null_mut(),
        channel_count: 2,
        latency: 0,
        constant_mask: 0,
    };
    let mut out_l = vec![0.0f32; frames];
    let mut out_r = vec![0.0f32; frames];
    let mut out_ptrs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
    let mut out_buf = clap_audio_buffer {
        data32: out_ptrs.as_mut_ptr(),
        data64: null_mut(),
        channel_count: 2,
        latency: 0,
        constant_mask: 0,
    };
    let p = clap_process {
        steady_time: 0,
        frames_count: frames as u32,
        transport: null(),
        audio_inputs: &in_buf,
        audio_outputs: &mut out_buf,
        audio_inputs_count: 1,
        audio_outputs_count: 1,
        in_events: null(),
        out_events: null(),
    };
    ((*plugin).process.unwrap())(plugin, &p);
    (out_l, out_r)
}

#[test]
fn patch_modulates_dc_input() {
    unsafe {
        let desc = Box::leak(Box::new(ClapDescriptor::new::<PdTremolo>()));
        let plugin = skuiz_clap::instantiate::<PdTremolo>(&desc.raw, null());
        assert!(((*plugin).init.unwrap())(plugin));

        // The engine delays by one 64-frame Pd tick, and the host must be
        // told the same number before and after activation.
        let ext = ((*plugin).get_extension.unwrap())(plugin, CLAP_EXT_LATENCY.as_ptr());
        assert!(!ext.is_null(), "latency extension must be exposed");
        let latency = &*(ext as *const clap_plugin_latency);
        let get = latency.get.unwrap();
        assert_eq!(get(plugin), 64, "one Pd tick, before activation");

        assert!(((*plugin).activate.unwrap())(plugin, 48_000.0, 1, 512));
        assert_eq!(get(plugin), 64, "same latency once the engine exists");

        // Silence in must stay silence out — a miswired patch that adds a DC
        // or noise source would fail here.
        let (l, r) = process_block(plugin, &mut [0.0f32; 512], &mut [0.0f32; 512]);
        assert!(l.iter().all(|s| s.abs() < 1e-6), "silence in, {l:?} out");
        assert!(r.iter().all(|s| s.abs() < 1e-6));

        // Full-scale DC on the left, silence on the right. At the default
        // depth the patch sweeps gain over 0.5..1.0 at 5 Hz; 40 blocks at
        // 512 frames / 48 kHz cover more than two LFO periods.
        let mut left = Vec::new();
        let mut right = Vec::new();
        for _ in 0..40 {
            let (l, r) = process_block(plugin, &mut [1.0f32; 512], &mut [0.0f32; 512]);
            left.extend_from_slice(&l);
            right.extend_from_slice(&r);
        }

        // Skip the engine's 64-frame priming latency plus oscillator startup.
        let steady = &left[8 * 512..];
        let min = steady.iter().copied().fold(f32::INFINITY, f32::min);
        let max = steady.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        assert!(max > 0.95, "tremolo peak {max} should approach unity gain");
        assert!(min < 0.6, "tremolo trough {min} should approach half gain");

        // The silent right channel must stay silent: the channels share an
        // LFO, not audio.
        assert!(
            right.iter().all(|s| s.abs() < 1e-6),
            "left channel leaked into the right"
        );

        ((*plugin).destroy.unwrap())(plugin);
    }
}

/// Depth 0 must make the patch transparent, which can only happen if
/// `set_param`'s `send_float` really reaches the patch's `[r depth]`.
#[test]
fn parameter_reaches_patch_receivers() {
    let mut proc_ = PdTremolo::default();
    proc_.activate(48_000.0, 512);

    let mut midi = skuiz_core::MidiOut::with_capacity(1);
    let run = |proc_: &mut PdTremolo, midi: &mut skuiz_core::MidiOut| {
        let mut l = [0.7f32; 512];
        let mut r = [0.7f32; 512];
        // Wire the main pair the way the adapters do after copy-in: the
        // input view aliases the output buffers.
        let mut scratch = skuiz_core::bus::TopologyScratch::new(PdTremolo::audio_buses());
        scratch.clear();
        scratch.set_active(skuiz_core::BusDirection::Input, 0, true);
        scratch.set_active(skuiz_core::BusDirection::Output, 0, true);
        unsafe {
            for (c, ch) in [&mut l, &mut r].into_iter().enumerate() {
                let ptr = ch.as_mut_ptr();
                scratch.set_channel(skuiz_core::BusDirection::Output, 0, c, ptr, 512);
                scratch.set_channel(skuiz_core::BusDirection::Input, 0, c, ptr, 512);
            }
        }
        let (inputs, mut outputs) = scratch.views();
        proc_.process(&inputs, &mut outputs, midi);
        (l, r)
    };

    // Default depth 1: the DC must be modulated, not passed through.
    let mut modulated = false;
    for _ in 0..40 {
        let (l, _) = run(&mut proc_, &mut midi);
        if l.iter().any(|s| (*s - 0.7).abs() > 0.05) {
            modulated = true;
        }
    }
    assert!(
        modulated,
        "default depth should audibly modulate a DC input"
    );

    // Live parameter change to depth 0: after a settle block, transparent.
    proc_.set_param(1, 0.0); // P_DEPTH
    let _ = run(&mut proc_, &mut midi);
    for i in 0..4 {
        let (l, r) = run(&mut proc_, &mut midi);
        for (s, out) in l.iter().zip(&r) {
            assert!(
                (*s - 0.7).abs() < 1e-5 && (*out - 0.7).abs() < 1e-5,
                "block {i}: depth 0 should pass 0.7 unchanged, got {s} / {out}"
            );
        }
    }
}
