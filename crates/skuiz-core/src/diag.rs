//! Diagnostics counters: every silent-drop path bumps one (invariant 8).
//!
//! Counters are plain atomics, so bumping one is realtime-safe. They are
//! never printed from the audio thread — reporting is main-thread work:
//! editors can pull a snapshot by posting
//! [`crate::protocol::DIAG_QUERY`].

use std::sync::atomic::{AtomicU64, Ordering};

/// Per-instance counters for every place a bounded structure can overflow
/// or a message can be lost. One per plugin instance, shared with the bus
/// callback via `Arc` where needed.
#[derive(Default)]
pub struct DiagCounters {
    /// Host automation points dropped because the block's staging buffer
    /// was full.
    pub param_events_dropped: AtomicU64,
    /// MIDI events dropped because `MidiOut` was full.
    pub midi_events_dropped: AtomicU64,
    /// Commands (editor/IPC/state) dropped because the command queue was
    /// full.
    pub commands_dropped: AtomicU64,
    /// Bus frames lost while no cross-process link existed (election
    /// window) or a write failed.
    pub bus_frames_dropped: AtomicU64,
    /// Seqlock retries in `ParamMirror::snapshot`. Not an error — a high
    /// number just means the publisher is being hammered.
    pub mirror_retries: AtomicU64,
}

impl DiagCounters {
    /// Realtime-safe increment.
    pub fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Read them all: `(name, value)` pairs, for tests and the debug dump.
    pub fn snapshot(&self) -> [(&'static str, u64); 5] {
        [
            (
                "param_events_dropped",
                self.param_events_dropped.load(Ordering::Relaxed),
            ),
            (
                "midi_events_dropped",
                self.midi_events_dropped.load(Ordering::Relaxed),
            ),
            (
                "commands_dropped",
                self.commands_dropped.load(Ordering::Relaxed),
            ),
            (
                "bus_frames_dropped",
                self.bus_frames_dropped.load(Ordering::Relaxed),
            ),
            (
                "mirror_retries",
                self.mirror_retries.load(Ordering::Relaxed),
            ),
        ]
    }
}
