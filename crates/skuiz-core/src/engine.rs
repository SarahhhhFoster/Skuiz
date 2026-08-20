//! The engine: the thread-ownership protocol every adapter shares.
//!
//! One [`Engine`] per plugin instance owns the processor and everything
//! the audio thread touches. The contract (docs/concepts/invariants.md):
//!
//! - the audio thread owns the processor while processing is active;
//! - the main thread may touch it only while processing is provably
//!   stopped (activate/deactivate, state ops, editor seeding);
//! - while processing runs, every other thread talks to the audio thread
//!   through bounded queues ([`crate::rt`]), and reads parameter values
//!   from the [`ParamMirror`] — never from the processor itself.
//!
//! The handoff is a three-state atomic: `IDLE` (nobody), `MAIN` (main
//! thread inside a direct access), `AUDIO` (blocks are flowing). The only
//! spin is in [`Engine::begin_audio`], which runs on the audio thread
//! *before* the first block — no deadline exists yet, so waiting out an
//! in-flight main-thread access there is safe.
//!
//! Structural safety: entering the AUDIO state yields an [`AudioToken`],
//! and every audio-thread entry point ([`Engine::audio_core`],
//! [`Engine::midi_out`]) requires a borrow of one. The token cannot be
//! constructed outside this module, cannot be cloned, and is consumed by
//! [`Engine::end_audio`] — so safe code cannot reach the processor on the
//! audio thread without provably holding the AUDIO state.

use std::cell::{RefCell, UnsafeCell};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::diag::DiagCounters;
use crate::rt::{
    command_queue, spsc, Command, CommandConsumer, CommandProducer, Consumer, ParamMirror,
    Producer, StateCommand, STATE_COMMAND_CAPACITY,
};
use crate::{MidiOut, Processor};

const IDLE: u8 = 0;
const MAIN: u8 = 1;
const AUDIO: u8 = 2;

/// How long a main-thread state op waits for the audio thread to answer
/// before giving up and telling the host the operation failed. Generous
/// compared to any block period, short enough to not wedge a host.
const STATE_ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(2);

/// How many in-flight state responses the audio thread can owe the main
/// thread. State ops are rare; two is already generous.
const STATE_RESPONSE_CAPACITY: usize = 2;

/// A state-operation answer from the audio thread.
enum StateResponse {
    /// `save_state` output.
    Saved(Vec<u8>),
    /// `load_state` result, plus the payload buffer handed back for reuse
    /// so the audio thread never frees it.
    Loaded { ok: bool, buffer: Vec<u8> },
}

/// Audio-thread-side state: the processor and everything `process` needs.
/// Reachable only under the access protocol — see [`Engine`].
pub struct AudioCore<P: Processor> {
    /// The plugin itself.
    pub processor: P,
    /// MIDI scratch for the current block, preallocated.
    pub midi_out: MidiOut,
    cmd_rx: CommandConsumer,
    state_cmd_rx: Consumer<StateCommand>,
    state_tx: Producer<StateResponse>,
}

/// Proof that the holder entered the AUDIO state via [`Engine::begin_audio`]
/// and has not left it. Not constructible outside the engine module, not
/// `Clone`, and consumed by [`Engine::end_audio`], so at most one exists
/// and only while blocks are flowing. Adapters stash it in instance state
/// for the duration of processing.
pub struct AudioToken(());

impl std::fmt::Debug for AudioToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AudioToken")
    }
}

/// What [`Engine::drain_commands`] did at the top of a block.
#[derive(Default)]
pub struct DrainReport {
    /// A remote (editor/IPC) or state-load change landed this block; the
    /// adapter should bounce to the main thread for a host rescan and
    /// editor refresh.
    pub notify_main: bool,
}

/// Cloneable handle for threads that must reach a (possibly still alive,
/// possibly gone) instance: the bus callback captures this. The `Weak`
/// fails to upgrade after instance destroy, so a frame in flight can never
/// use-after-free — the command just goes nowhere.
pub struct EngineHandle<P: Processor> {
    engine: std::sync::Weak<Engine<P>>,
    /// Shared diagnostics; also outlives the instance for the same reason.
    pub diag: Arc<DiagCounters>,
}

impl<P: Processor> Clone for EngineHandle<P> {
    fn clone(&self) -> Self {
        Self {
            engine: self.engine.clone(),
            diag: Arc::clone(&self.diag),
        }
    }
}

impl<P: Processor> EngineHandle<P> {
    /// Route a parameter change to the instance (direct when stopped,
    /// queued when running). Returns `false` when the change did not enter
    /// the engine — instance gone, or the command queue full (counted via
    /// `commands_dropped`) — so callers tracking versions never record a
    /// change that went nowhere (invariant 9).
    pub fn set_param(&self, id: u32, value: f64) -> bool {
        match self.engine.upgrade() {
            Some(engine) => engine.set_param(id, value),
            None => false,
        }
    }

    /// Read back the current mirrored parameter values. Used to answer IPC
    /// `sync_request` frames; empty after the instance is gone.
    pub fn snapshot_params(&self) -> Vec<(u32, f64)> {
        self.engine
            .upgrade()
            .map(|engine| engine.mirror().snapshot())
            .unwrap_or_default()
    }
}

/// Per-instance engine. `Sync` is unsafe-implemented: correctness rests on
/// the access protocol — only the thread holding the current state touches
/// `core`, and state transitions carry acquire/release ordering.
pub struct Engine<P: Processor> {
    core: UnsafeCell<AudioCore<P>>,
    access: AtomicU8,
    mirror: ParamMirror,
    diag: Arc<DiagCounters>,
    cmd_tx: CommandProducer,
    /// State-op commands, single-producer by way of this mutex. The lock is
    /// held for the whole round-trip (push + wait for the response), so at
    /// most one state op is ever in flight — which is the proof that the
    /// audio thread's response ring (capacity 2) cannot overflow. Main
    /// thread only.
    state_cmd_tx: Mutex<Producer<StateCommand>>,
    /// Main-thread only, hence RefCell: the response half of state ops.
    state_rx: RefCell<Consumer<StateResponse>>,
    /// Main-thread stash of freed state buffers, reused across loads.
    buffers: RefCell<Vec<Vec<u8>>>,
    /// Cached at `activate`; the host's latency query reads it without
    /// touching the processor at all.
    latency: std::sync::atomic::AtomicU32,
    /// Set when the audio thread observes the processor's latency differ
    /// from the reported value; consumed on the main thread, which tells
    /// the host (CLAP `clap_host_latency.changed`, VST3
    /// `kLatencyChanged`).
    latency_changed: std::sync::atomic::AtomicBool,
}

// SAFETY: `core` is touched only by the thread holding the access state
// (protocol above); every other field is thread-safe on its own.
unsafe impl<P: Processor> Send for Engine<P> {}
unsafe impl<P: Processor> Sync for Engine<P> {}

impl<P: Processor + Default> Engine<P> {
    /// Build an engine around a fresh processor, inside the `Arc` the
    /// handle's `Weak` points at. Main thread (instance setup). The mirror
    /// starts at the processor's initial values.
    pub fn new(midi_capacity: usize) -> Arc<Self> {
        let processor = P::default();
        let mirror = ParamMirror::new(P::params(), |id| processor.get_param(id));
        // Seed the latency cache at construction: hosts may query latency
        // before activate, and the processor's static latency is already
        // valid then (activate refreshes it for sample-rate-dependent DSP).
        let initial_latency = processor.latency();
        let (cmd_tx, cmd_rx) = command_queue();
        let (state_cmd_tx, state_cmd_rx) = spsc(STATE_COMMAND_CAPACITY);
        let (state_tx, state_rx) = spsc(STATE_RESPONSE_CAPACITY);
        Arc::new(Self {
            core: UnsafeCell::new(AudioCore {
                processor,
                midi_out: MidiOut::with_capacity(midi_capacity),
                cmd_rx,
                state_cmd_rx,
                state_tx,
            }),
            access: AtomicU8::new(IDLE),
            mirror,
            diag: Arc::new(DiagCounters::default()),
            cmd_tx,
            state_cmd_tx: Mutex::new(state_cmd_tx),
            state_rx: RefCell::new(state_rx),
            buffers: RefCell::new(Vec::new()),
            latency: std::sync::atomic::AtomicU32::new(initial_latency),
            latency_changed: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

impl<P: Processor> Engine<P> {
    /// The cloneable handle for bus callbacks and editor closures.
    pub fn handle(self: &Arc<Self>) -> EngineHandle<P> {
        EngineHandle {
            engine: Arc::downgrade(self),
            diag: Arc::clone(&self.diag),
        }
    }

    /// Diagnostics counters for this instance.
    pub fn diag(&self) -> &DiagCounters {
        &self.diag
    }

    /// Wait-free parameter reads for any thread (invariant 6).
    pub fn mirror(&self) -> &ParamMirror {
        &self.mirror
    }

    // --- the access protocol ----------------------------------------------

    /// Enter the AUDIO state, returning the token that proves it. Called
    /// from `start_processing` (CLAP) / `setProcessing(true)` (VST3) —
    /// before the first block, so no deadline exists while this spins out
    /// an in-flight main access. The caller keeps the token in instance
    /// state and hands it back to [`Engine::end_audio`].
    pub fn begin_audio(&self) -> AudioToken {
        debug_assert!(
            self.access.load(Ordering::Relaxed) != AUDIO,
            "begin_audio while already AUDIO"
        );
        while self
            .access
            .compare_exchange_weak(IDLE, AUDIO, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
        }
        AudioToken(())
    }

    /// Leave the AUDIO state (`stop_processing`). Audio thread; consumes
    /// the token, so no audio-core access is possible afterwards.
    pub fn end_audio(&self, _token: AudioToken) {
        self.access.store(IDLE, Ordering::Release);
    }

    /// Whether blocks are flowing. Any thread.
    pub fn is_processing(&self) -> bool {
        self.access.load(Ordering::Acquire) == AUDIO
    }

    /// The audio-side state. Audio thread only, and only with a borrow of
    /// the live [`AudioToken`] — safe code cannot reach this without one.
    #[allow(clippy::mut_from_ref)]
    pub fn audio_core(&self, _token: &AudioToken) -> &mut AudioCore<P> {
        debug_assert!(
            self.access.load(Ordering::Relaxed) == AUDIO,
            "audio_core without begin_audio"
        );
        // SAFETY: the token proves the caller entered the AUDIO state and
        // has not left it, so no other thread may touch `core` (protocol at
        // the top of this module).
        unsafe { &mut *self.core.get() }
    }

    /// Run `f` with direct processor access, but only if the transport is
    /// stopped. Returns `None` while blocks are flowing — the caller falls
    /// back to the command queue. Main thread.
    pub fn with_main<R>(&self, f: impl FnOnce(&mut AudioCore<P>) -> R) -> Option<R> {
        if self
            .access
            .compare_exchange(IDLE, MAIN, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }
        // Drop guard: a panic in `f` must not wedge the state machine
        // (parity with the old poisoned-mutex recovery).
        struct Reset<'a>(&'a AtomicU8);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.store(IDLE, Ordering::Release);
            }
        }
        let _reset = Reset(&self.access);
        // SAFETY: we hold the MAIN state; the audio thread cannot hold
        // AUDIO concurrently (CAS above), and hosts do not call
        // start_processing concurrently with main-thread callbacks.
        let core = unsafe { &mut *self.core.get() };
        Some(f(core))
    }

    // --- main-thread entry points -----------------------------------------

    /// A parameter change from the editor or the bus. Applied immediately
    /// when the transport is stopped, queued for the next block otherwise.
    /// Main or bus thread; realtime-safe by construction (never locks the
    /// processor).
    ///
    /// Returns `false` when the change went nowhere (command queue full;
    /// counted via `commands_dropped`). Callers tracking versions (LWW)
    /// must only record a change that returned `true` — otherwise the
    /// version is marked accepted for a value that never landed, and
    /// convergence breaks (invariant 9).
    pub fn set_param(&self, id: u32, value: f64) -> bool {
        let applied = self.with_main(|core| {
            core.processor.set_param(id, value);
            // Publish the readback, not the request: processors may round
            // or clamp, and the mirror must reflect the value actually in
            // force (hosts diff it against serialized state).
            core.processor.get_param(id)
        });
        match applied {
            Some(actual) => {
                self.mirror.publish(id, actual);
                true
            }
            None => match self.cmd_tx.push(Command::SetParam { id, value }) {
                Ok(()) => true,
                Err(_) => {
                    DiagCounters::bump(&self.diag.commands_dropped);
                    false
                }
            },
        }
    }

    /// Serialize state for the host. Main thread. Direct when stopped;
    /// a bounded round-trip through the audio thread when running.
    /// `None` means the round-trip timed out — tell the host it failed.
    pub fn save_state(&self) -> Option<Vec<u8>> {
        if let Some(data) = self.with_main(|core| core.processor.save_state()) {
            return Some(data);
        }
        // Serialize state ops: holding this lock across the round-trip
        // guarantees at most one is in flight, which is what keeps the
        // audio thread's response ring (capacity 2) from ever overflowing.
        let mut state_tx = self.state_cmd_tx.lock().unwrap_or_else(|e| e.into_inner());
        state_tx.push(StateCommand::SaveState).ok()?;
        match self.wait_for_response()? {
            StateResponse::Saved(data) => Some(data),
            StateResponse::Loaded { .. } => None, // protocol bug; fail loud-ish
        }
    }

    /// Restore host state. Main thread; same dual path as `save_state`.
    pub fn load_state(&self, data: Vec<u8>) -> bool {
        if let Some(ok) = self.with_main(|core| {
            let ok = core.processor.load_state(&data);
            if ok {
                // Publish from inside the access: the CAS in with_main would
                // reject a nested attempt (we already hold MAIN).
                let values: Vec<(u32, f64)> = P::params()
                    .iter()
                    .map(|d| (d.id, core.processor.get_param(d.id)))
                    .collect();
                self.mirror.publish_all(&values);
            }
            ok
        }) {
            return ok;
        }
        // Reuse a stashed buffer when we have one; the audio thread must
        // never free or allocate these (invariant 3).
        let payload = {
            let mut stash = self.buffers.borrow_mut();
            match stash.pop() {
                Some(mut buf) => {
                    buf.clear();
                    buf.extend_from_slice(&data);
                    buf
                }
                None => data,
            }
        };
        // Same one-op-in-flight serialization as save_state.
        let mut state_tx = self.state_cmd_tx.lock().unwrap_or_else(|e| e.into_inner());
        if state_tx.push(StateCommand::LoadState(payload)).is_err() {
            DiagCounters::bump(&self.diag.commands_dropped);
            return false;
        }
        match self.wait_for_response() {
            // The audio thread already republished the mirror on success.
            Some(StateResponse::Loaded { ok, buffer }) => {
                self.buffers.borrow_mut().push(buffer);
                ok
            }
            _ => false,
        }
    }

    /// Spin (main thread, bounded) until the audio thread answers a state
    /// command. Realtime rules don't apply here — this thread is allowed to
    /// wait — but the audio thread answering must never block.
    fn wait_for_response(&self) -> Option<StateResponse> {
        let deadline = Instant::now() + STATE_ROUND_TRIP_TIMEOUT;
        loop {
            if let Some(resp) = self.state_rx.borrow_mut().pop() {
                return Some(resp);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::yield_now();
        }
    }

    /// Record the latency to report to hosts. Main thread (activate).
    pub fn set_latency(&self, latency: u32) {
        self.latency.store(latency, Ordering::Relaxed);
    }

    /// Reset DSP state (`Processor::reset`). Main thread. Direct when
    /// stopped; queued for the top of the next block when running, so the
    /// reset lands between blocks rather than mid-buffer. Returns `false`
    /// when the command queue was full (counted via `commands_dropped`).
    pub fn reset(&self) -> bool {
        let applied = self.with_main(|core| core.processor.reset());
        match applied {
            Some(()) => true,
            None => match self.cmd_tx.push(Command::Reset) {
                Ok(()) => true,
                Err(_) => {
                    DiagCounters::bump(&self.diag.commands_dropped);
                    false
                }
            },
        }
    }

    /// The cached latency for the host's query — any thread, wait-free.
    pub fn latency(&self) -> u32 {
        self.latency.load(Ordering::Relaxed)
    }

    /// Whether the audio thread reported a latency change since the last
    /// call; reading clears the flag. Main thread — the adapter consumes
    /// this where it can reach the host (CLAP `on_main_thread`, VST3
    /// `getParamNormalized`).
    pub fn take_latency_changed(&self) -> bool {
        self.latency_changed.swap(false, Ordering::Acquire)
    }

    // --- audio-thread entry point ------------------------------------------

    /// The MIDI scratch of the last rendered block. Unlike the rest of the
    /// core, no `with_main` closure in any adapter touches this buffer — it
    /// is written only by the render thread (`process`/`clear`) — so the
    /// render thread may re-claim the audio state to read the events it
    /// just produced. AUv3 needs exactly that: the shim enumerates MIDI
    /// output after the render call returns but inside the same host render
    /// block, so the adapter briefly re-enters AUDIO (`begin_audio` → read
    /// → `end_audio`) on the render thread rather than holding the token
    /// across render calls.
    pub fn midi_out(&self, _token: &AudioToken) -> &MidiOut {
        // SAFETY: per the contract above, `midi_out` has a single writer
        // (the render thread), so a render-thread read never races.
        unsafe { &(*self.core.get()).midi_out }
    }

    /// Drain pending commands at the top of a block. Audio thread; the
    /// caller holds AUDIO and passes the core from [`Engine::audio_core`].
    ///
    /// Ordinary commands (parameter changes, resets) are cheap and bounded,
    /// so the queue is drained in full. State commands are potentially
    /// expensive (whole-processor (de)serialization) and travel on their
    /// own queue: at most one is serviced per block, so neither a flood of
    /// parameter moves nor a heavyweight state op can extend this callback
    /// beyond one bounded amount of work. Lock-free and allocation-free
    /// apart from the documented state-op exception (invariant 3 names it:
    /// host-initiated (de)serialization allocates inside the plugin's
    /// `save_state`).
    pub fn drain_commands(&self, core: &mut AudioCore<P>) -> DrainReport {
        let mut report = DrainReport::default();
        while let Some(cmd) = core.cmd_rx.pop() {
            match cmd {
                Command::SetParam { id, value } => {
                    core.processor.set_param(id, value);
                    // Publish the readback, not the request (processors may
                    // round/clamp), and publish straight away: collecting
                    // into a batch Vec would allocate on the audio thread
                    // (invariant 3).
                    self.mirror.publish(id, core.processor.get_param(id));
                    report.notify_main = true;
                }
                Command::Reset => {
                    core.processor.reset();
                }
            }
        }
        // At most one state op per block: these can be arbitrarily heavy,
        // and the queue holds at most one more behind it.
        if let Some(cmd) = core.state_cmd_rx.pop() {
            match cmd {
                StateCommand::LoadState(data) => {
                    let ok = core.processor.load_state(&data);
                    // Hand the buffer back for reuse; freeing it here would
                    // put an allocator call on the audio thread. The push
                    // cannot fail while the state-op lock serializes
                    // round-trips (capacity 2, at most one in flight) — the
                    // counter exists to catch a violation of that proof.
                    if core
                        .state_tx
                        .push(StateResponse::Loaded { ok, buffer: data })
                        .is_err()
                    {
                        debug_assert!(false, "state response ring overflowed");
                        DiagCounters::bump(&self.diag.state_responses_dropped);
                    }
                    if ok {
                        // A load rewrote everything: republish the whole
                        // mirror from the processor. Reading it here is fine
                        // — the audio thread owns it during a block. One
                        // publish per parameter: batching into a Vec would
                        // allocate on the audio thread (invariant 3).
                        for def in P::params() {
                            self.mirror
                                .publish(def.id, core.processor.get_param(def.id));
                        }
                        report.notify_main = true;
                    }
                }
                StateCommand::SaveState => {
                    let data = core.processor.save_state();
                    if core.state_tx.push(StateResponse::Saved(data)).is_err() {
                        debug_assert!(false, "state response ring overflowed");
                        DiagCounters::bump(&self.diag.state_responses_dropped);
                    }
                }
            }
        }
        // Dynamic latency: a processor may change its latency at runtime
        // (a lookahead limiter engaging, say). One atomic load per block
        // when nothing changed; on change, update the cached value and ask
        // for the main-thread bounce so the adapter can tell the host.
        let latency = core.processor.latency();
        if latency != self.latency.load(Ordering::Relaxed) {
            self.latency.store(latency, Ordering::Relaxed);
            self.latency_changed.store(true, Ordering::Release);
            report.notify_main = true;
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ParamDef, PluginInfo};

    struct Gain(f64);
    impl Default for Gain {
        fn default() -> Self {
            Gain(0.5)
        }
    }
    impl Processor for Gain {
        fn info() -> PluginInfo {
            PluginInfo {
                id: "test.gain",
                name: "g",
                vendor: "t",
                version: "0",
                description: "",
            }
        }
        fn params() -> &'static [ParamDef] {
            &[ParamDef {
                id: 7,
                name: "gain",
                min: 0.0,
                max: 1.0,
                default: 0.5,
                choices: &[],
                shared: true,
            }]
        }
        fn set_param(&mut self, _id: u32, v: f64) {
            self.0 = v;
        }
        fn get_param(&self, _id: u32) -> f64 {
            self.0
        }
        fn process(&mut self, _channels: &mut [&mut [f32]], _midi: &mut MidiOut) {}
    }

    #[test]
    fn stopped_engine_applies_directly() {
        let engine = Engine::<Gain>::new(64);
        assert!(!engine.is_processing());
        assert!(engine.set_param(7, 0.9));
        assert_eq!(engine.mirror().get(7), Some(0.9));
        let data = engine.save_state().unwrap();
        assert!(engine.load_state(data));
    }

    #[test]
    fn running_engine_round_trips_state_and_params() {
        let engine = Engine::<Gain>::new(64);
        let token = engine.begin_audio();
        assert!(engine.is_processing());

        // "Audio thread": drains until the main thread says stop — like a
        // host calling process() for as long as the transport runs.
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let audio = {
            let engine = Arc::clone(&engine);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut saw_notify = false;
                while !stop.load(Ordering::Relaxed) {
                    let core = engine.audio_core(&token);
                    saw_notify |= engine.drain_commands(core).notify_main;
                }
                (saw_notify, token)
            })
        };

        // "Main thread": param change and a state round-trip while running.
        assert!(engine.set_param(7, 0.25));
        let saved = engine.save_state().expect("save round-trip timed out");
        assert!(engine.load_state(saved));
        assert_eq!(engine.mirror().get(7), Some(0.25));

        stop.store(true, Ordering::Relaxed);
        let (saw_notify, token) = audio.join().unwrap();
        assert!(saw_notify, "the queued change set notify_main");
        engine.end_audio(token);
    }

    /// A state op must not starve behind a full param queue, and a pending
    /// state op must not occupy param-queue slots: the queues are separate,
    /// so a round-trip answers even with the param queue packed full.
    #[test]
    fn state_round_trip_works_with_a_full_param_queue() {
        let engine = Engine::<Gain>::new(64);
        let token = engine.begin_audio();

        // Pack the param queue to capacity; the next push fails.
        for _ in 0..crate::rt::COMMAND_CAPACITY {
            assert!(engine.set_param(7, 0.5));
        }
        assert!(!engine.set_param(7, 0.5), "queue full: push reports it");

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        std::thread::scope(|s| {
            // "Audio thread": render blocks until told to stop.
            let engine = &engine;
            let token = &token;
            let stop = &stop;
            let drainer = s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    engine.drain_commands(engine.audio_core(token));
                    std::thread::yield_now();
                }
            });
            // A save round-trip still works — it never touches the param
            // queue, and the drainer services the state queue alongside.
            let saved = engine.save_state().expect("save with full param queue");
            assert!(engine.load_state(saved));
            stop.store(true, Ordering::Relaxed);
            drainer.join().unwrap();
        });

        assert_eq!(engine.mirror().get(7), Some(0.5));
        assert!(
            engine.diag().commands_dropped.load(Ordering::Relaxed) >= 1,
            "the failed push was counted"
        );
        engine.end_audio(token);
    }

    /// State ops are serviced at most once per block and serialize on the
    /// state-op lock: two concurrent round-trips (an adversarial host) both
    /// complete, and the response ring never overflows.
    #[test]
    fn concurrent_state_round_trips_serialize_and_complete() {
        let engine = Engine::<Gain>::new(64);
        let token = engine.begin_audio();
        let a = std::thread::spawn({
            let engine = Arc::clone(&engine);
            move || engine.save_state()
        });
        let b = std::thread::spawn({
            let engine = Arc::clone(&engine);
            move || engine.save_state()
        });
        // "Audio thread": keep rendering blocks until both round-trips have
        // had ample time to complete (well under their internal timeout).
        for _ in 0..100 {
            engine.drain_commands(engine.audio_core(&token));
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(a.join().unwrap().is_some(), "first round-trip completed");
        assert!(b.join().unwrap().is_some(), "second round-trip completed");
        assert_eq!(
            engine
                .diag()
                .state_responses_dropped
                .load(Ordering::Relaxed),
            0,
            "the one-in-flight proof held"
        );
        engine.end_audio(token);
    }

    /// The LWW/queue atomicity regression: an IPC frame whose command could
    /// not enter the engine must leave no version mark, so the identical
    /// version re-delivered later (a sync_state heal) still wins and the
    /// instance converges instead of diverging permanently (invariant 9).
    #[test]
    fn ipc_frame_dropped_by_full_queue_is_not_marked_and_heals() {
        use crate::lww::Lww;
        use crate::rt::COMMAND_CAPACITY;

        let engine = Engine::<Gain>::new(64);
        let token = engine.begin_audio();
        let lww = Lww::new();

        // Fill the command queue with accepted frames.
        for seq in 1..=COMMAND_CAPACITY as u64 {
            assert!(lww.accept_with(7, Some((seq, 1)), || engine.set_param(7, 0.1)));
        }
        // The queue is full: this frame's command goes nowhere...
        let lost = (COMMAND_CAPACITY as u64 + 1, 1);
        assert!(!lww.accept_with(7, Some(lost), || engine.set_param(7, 0.9)));
        // ...and it must NOT be marked accepted, or the heal below would be
        // rejected as stale and the instance would stay diverged forever.
        assert_eq!(
            lww.known_version(7),
            Some((COMMAND_CAPACITY as u64, 1)),
            "the lost frame left no mark"
        );

        // Heal: the queue drains, then the same version arrives again (as a
        // sync_state answer would re-deliver it).
        let core = engine.audio_core(&token);
        engine.drain_commands(core);
        assert!(
            lww.accept_with(7, Some(lost), || engine.set_param(7, 0.9)),
            "the re-delivered version still wins"
        );
        let core = engine.audio_core(&token);
        engine.drain_commands(core);
        assert_eq!(engine.mirror().get(7), Some(0.9), "converged");
        engine.end_audio(token);
    }

    /// An editor-originated change whose command cannot enter the engine is
    /// neither stamped nor broadcast: the instance never claims a version
    /// for a value it does not hold.
    #[test]
    fn local_edit_dropped_by_full_queue_is_not_stamped() {
        use crate::lww::Lww;
        use crate::rt::COMMAND_CAPACITY;

        let engine = Engine::<Gain>::new(64);
        let token = engine.begin_audio();
        let lww = Lww::new();
        for _ in 0..COMMAND_CAPACITY {
            assert!(engine.set_param(7, 0.5));
        }
        assert_eq!(lww.stamp_with(7, || engine.set_param(7, 0.9)), None);
        assert_eq!(lww.known_version(7), None, "no version was claimed");
        engine.end_audio(token);
    }

    /// Simultaneous editors: the LWW record lock serializes apply order
    /// with version order, so the engine must end up holding the value that
    /// carries the highest version — never a stale one with a fresh mark.
    #[test]
    fn simultaneous_editors_converge_on_the_highest_version() {
        use crate::lww::Lww;

        let engine = Engine::<Gain>::new(64);
        let token = engine.begin_audio();
        let lww = Arc::new(Lww::new());
        const EDITORS: usize = 8;
        let threads: Vec<_> = (0..EDITORS)
            .map(|n| {
                let engine = Arc::clone(&engine);
                let lww = Arc::clone(&lww);
                std::thread::spawn(move || {
                    let value = 0.5 + n as f64 * 0.01;
                    lww.stamp_with(7, || engine.set_param(7, value))
                        .map(|ver| (ver, value))
                })
            })
            .collect();
        let stamped: Vec<_> = threads
            .into_iter()
            .map(|t| t.join().unwrap().expect("queue has room for 8 edits"))
            .collect();
        let core = engine.audio_core(&token);
        engine.drain_commands(core);
        engine.end_audio(token);

        let (best_version, best_value) = stamped.iter().max_by_key(|(v, _)| *v).unwrap();
        assert_eq!(engine.mirror().get(7), Some(*best_value));
        assert_eq!(lww.known_version(7), Some(*best_version));
    }

    /// Owner death during sync: the engine handle must fail cleanly rather
    /// than touch freed state.
    #[test]
    fn handle_outlives_engine_safely() {
        let handle = {
            let engine = Engine::<Gain>::new(64);
            engine.handle()
            // engine dropped here
        };
        assert!(!handle.set_param(7, 0.5), "gone instance: not accepted");
        assert!(handle.snapshot_params().is_empty());
    }

    /// Repeated lifecycle transitions with traffic in between: every cycle
    /// must end with the queued changes applied exactly once.
    #[test]
    fn repeated_start_stop_cycles_apply_queued_changes() {
        let engine = Engine::<Gain>::new(64);
        for cycle in 0..20 {
            let token = engine.begin_audio();
            let want = (cycle as f64 + 1.0) / 100.0;
            assert!(engine.set_param(7, want));
            assert!(engine.reset());
            let core = engine.audio_core(&token);
            engine.drain_commands(core);
            engine.end_audio(token);
            assert_eq!(engine.mirror().get(7), Some(want));
        }
    }

    /// Rapidly changing latency: every change is reported, none is lost,
    /// and the flag fires once per observed change.
    #[test]
    fn rapidly_changing_latency_is_reported_each_time() {
        let engine = Engine::<DynLatency>::new(64);
        engine.set_latency(0);
        let token = engine.begin_audio();
        let mut prev = 0;
        for want in [0, 64, 0, 512, 384, 384, 0] {
            engine.audio_core(&token).processor.0 = want;
            let report = engine.drain_commands(engine.audio_core(&token));
            assert_eq!(engine.latency(), want);
            assert_eq!(report.notify_main, want != prev);
            prev = want;
        }
        engine.end_audio(token);
    }

    #[test]
    fn begin_audio_waits_out_a_main_access() {
        let engine = Engine::<Gain>::new(64);
        let audio = {
            let engine = Arc::clone(&engine);
            std::thread::spawn(move || {
                let token = engine.begin_audio();
                engine.end_audio(token);
            })
        };
        // Hold a main access briefly; the audio starter must wait, then win.
        engine.with_main(|core| {
            core.processor.set_param(7, 0.1);
            std::thread::sleep(Duration::from_millis(50));
        });
        audio.join().unwrap();
        assert!(!engine.is_processing());
    }

    /// A processor whose latency moves at runtime.
    #[derive(Default)]
    struct DynLatency(u32);
    impl Processor for DynLatency {
        fn info() -> PluginInfo {
            PluginInfo {
                id: "test.dynlat",
                name: "d",
                vendor: "t",
                version: "0",
                description: "",
            }
        }
        fn params() -> &'static [ParamDef] {
            &[]
        }
        fn set_param(&mut self, _id: u32, _v: f64) {}
        fn get_param(&self, _id: u32) -> f64 {
            0.0
        }
        fn latency(&self) -> u32 {
            self.0
        }
        fn process(&mut self, _channels: &mut [&mut [f32]], _midi: &mut MidiOut) {}
    }

    #[test]
    fn latency_change_is_reported_and_flagged_once() {
        let engine = Engine::<DynLatency>::new(64);
        // Simulate activate: report the initial latency.
        let initial = engine.with_main(|core| core.processor.latency()).unwrap();
        engine.set_latency(initial);

        let token = engine.begin_audio();
        let report = engine.drain_commands(engine.audio_core(&token));
        assert!(!report.notify_main, "unchanged latency must stay quiet");
        assert!(!engine.take_latency_changed());

        // The processor's latency moves (main thread, transport stopped is
        // not required for the test — the flag is what matters).
        engine.end_audio(token);
        engine.with_main(|core| core.processor.0 = 384);
        let token = engine.begin_audio();
        let report = engine.drain_commands(engine.audio_core(&token));
        assert!(report.notify_main, "a latency change asks for the bounce");
        assert_eq!(engine.latency(), 384, "the cached value updates at once");
        assert!(engine.take_latency_changed());
        assert!(!engine.take_latency_changed(), "the flag is consumed once");
        engine.end_audio(token);
    }
}
