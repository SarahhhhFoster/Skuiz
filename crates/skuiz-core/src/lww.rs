//! Last-writer-wins bookkeeping for shared parameters (invariant 9).
//!
//! Every bus frame carrying a parameter change is versioned with a lamport
//! clock and the sender's origin id. An instance applies an incoming value
//! only when its version is newer than the last one seen for that
//! parameter, so a dropped, duplicated, delayed or reordered frame can
//! delay convergence but never prevent it: the next newer frame — or a
//! [`crate::protocol::sync_state`] answer to a late joiner's
//! `sync_request` — always heals the gap.
//!
//! This tracker runs on the bus and main/UI threads only; the audio thread
//! never sees it (the engine's command queue is the boundary).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Per-parameter versions plus this instance's identity.
pub struct Lww {
    /// This instance's origin id: unique per process run, used to break
    /// sequence-number ties deterministically.
    origin: u64,
    /// Lamport clock: bumped on every local stamp, advanced on every
    /// accepted frame.
    clock: AtomicU64,
    /// Last accepted `(seq, origin)` per parameter. Mutex'd, but only ever
    /// taken on non-realtime threads (bus callback, editor IPC handler).
    last: Mutex<HashMap<u32, (u64, u64)>>,
}

impl Default for Lww {
    fn default() -> Self {
        Self::new()
    }
}

impl Lww {
    /// A tracker with a fresh origin id. The id mixes the process id with a
    /// global counter; uniqueness across simultaneously running processes
    /// is what matters, and versions never persist across runs.
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let origin = ((std::process::id() as u64) << 32) | NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            origin,
            clock: AtomicU64::new(0),
            last: Mutex::new(HashMap::new()),
        }
    }

    /// This instance's origin id, for `sync_request` frames.
    pub fn origin(&self) -> u64 {
        self.origin
    }

    /// Send path: stamp a local change with a fresh version and record it.
    pub fn stamp(&self, id: u32) -> (u64, u64) {
        let seq = self.clock.fetch_add(1, Ordering::Relaxed) + 1;
        self.last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, (seq, self.origin));
        (seq, self.origin)
    }

    /// Receive path: whether an incoming version wins over what was last
    /// seen for `id` (and if so, record it). Legacy unversioned frames —
    /// from old peers or hand-rolled senders — always apply, but leave no
    /// mark: the versioned record stays authoritative, so a legacy value can
    /// never displace known state permanently (the next `sync_state` round
    /// heals it), and stale versioned frames still lose afterwards.
    pub fn accept(&self, id: u32, version: Option<(u64, u64)>) -> bool {
        let Some((seq, origin)) = version else {
            return true;
        };
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let wins = match last.get(&id).copied() {
            Some((lseq, lorigin)) => (seq, origin) > (lseq, lorigin),
            None => true,
        };
        if wins {
            last.insert(id, (seq, origin));
            // Advance the clock so our next local edit wins against
            // anything we just learned about.
            self.clock.fetch_max(seq, Ordering::Relaxed);
        }
        wins
    }

    /// The recorded version for `id`, if any was ever stamped or accepted.
    /// `None` means this instance never saw a bus edit for the parameter —
    /// and a `sync_state` answer must omit such parameters entirely, because
    /// the current value may have come from host automation, which is
    /// per-instance and must not propagate to joiners (invariant 10).
    pub fn known_version(&self, id: u32) -> Option<(u64, u64)> {
        self.last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_win_and_stale_ones_lose() {
        let lww = Lww::new();
        assert!(lww.accept(0, Some((5, 10))));
        assert!(!lww.accept(0, Some((5, 10))), "duplicate is not newer");
        assert!(!lww.accept(0, Some((4, 99))), "older seq loses");
        assert!(lww.accept(0, Some((5, 11))), "same seq, higher origin wins");
        assert!(
            !lww.accept(0, Some((5, 10))),
            "tie resolved, former loses now"
        );
        assert!(lww.accept(0, Some((6, 1))));
    }

    #[test]
    fn legacy_frames_apply_without_touching_versions() {
        let lww = Lww::new();
        assert!(lww.accept(0, None));
        assert!(lww.accept(0, None), "legacy always applies");
        assert_eq!(lww.known_version(0), None, "legacy leaves no mark");
        let lww = Lww::new();
        assert!(lww.accept(0, Some((5, 1))));
        assert!(lww.accept(0, None), "legacy still applies over a version");
        assert_eq!(
            lww.known_version(0),
            Some((5, 1)),
            "but the versioned record stays authoritative"
        );
        assert!(
            !lww.accept(0, Some((4, 9))),
            "and stale versions still lose"
        );
    }

    #[test]
    fn local_stamps_always_beat_what_we_just_saw() {
        let lww = Lww::new();
        assert!(lww.accept(0, Some((10, 1))));
        let (seq, origin) = lww.stamp(0);
        assert!(seq > 10, "stamp must advance past the learned clock");
        assert_eq!(origin, lww.origin());
        assert_eq!(lww.known_version(0), Some((seq, origin)));
        // And a peer holding our stamped version does not re-win it back.
        assert!(!lww.accept(0, Some((seq, origin))));
    }

    #[test]
    fn unseen_params_have_no_version_to_answer_with() {
        let lww = Lww::new();
        assert_eq!(lww.known_version(3), None);
    }
}
