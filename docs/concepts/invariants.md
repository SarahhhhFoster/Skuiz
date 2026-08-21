# Invariants

The realtime and concurrency contract Skuiz holds itself to. These are
not aspirations or style guidance: code that violates one is buggy, full
stop. Each invariant states what it means, what enforces it, and where
the tree stands today — honestly, including where today falls short.

Status legend: **held** (mechanism in place, tested), **partial** (the
mechanism exists but has known gaps), **violating** (today's design
breaks it; the fix is scheduled P0/P1 work).

## 1. The audio thread owns the `Processor` during `process()`

No other thread may call into the processor while a block is rendering.
Ownership is structural — the processor lives in audio-side state that
only the audio-side entry points can reach (`process`, the CLAP
`params_flush`, between-block state-op servicing, and the `MidiOut`
accessor) — not mutual exclusion.

- **Enforced by:** `Engine`'s three-state access machine
  (`crates/skuiz-core/src/engine.rs`): the processor lives behind an
  `UnsafeCell` reachable only through `audio_core()`, which requires a
  borrow of an `AudioToken` — obtainable only from `begin_audio`, not
  constructible or cloneable outside the engine, and consumed by
  `end_audio`. Safe code therefore cannot reach the audio core without
  provably holding the AUDIO state; the main thread gets access only
  while the transport is stopped (`with_main`).
- **Status: held.** All four adapters route through the engine and no
  adapter code touches the processor outside the state machine. CLAP and
  VST3 stash the token in instance state between start/stop; AUv3 and
  the standalone have no start/stop pair, so they claim the token at the
  top of each render callback and hand it back at the bottom.

## 2. No mutex may be acquired by the audio thread

Not "briefly", not "probably uncontended". Zero lock acquisitions on
the audio thread, ever.

- **Enforced by:** the engine's lock-free SPSC command queue and atomic
  parameter mirror (`crates/skuiz-core/src/rt.rs`); the command queue's
  producer-side mutex is only ever taken on non-realtime threads (bus,
  UI), and the audio thread only pops.
- **Status: held.** The processor, `MidiOut`, param-event staging and
  the IPC ingress path are all mutex-free on the audio thread in every
  adapter.

## 3. No allocation may occur on the audio thread

Buffers are built in `activate`; every audio-thread structure is
preallocated and fixed-capacity.

- **Enforced by:** `MidiOut::with_capacity` (never reallocates —
  tested), preallocated param-event staging, bounded command queues
  allocated at instance setup, and state-payload buffers recycled back
  to the main thread so the audio thread never frees them.
- **Status: held, with one documented exception.** While the transport
  runs, host-initiated `save_state`/`load_state` must execute where the
  processor lives — the audio thread, between blocks — and the plugin's
  own `save_state` implementation returns a freshly allocated `Vec`.
  That allocation is inherent to the `Processor` trait's signature and
  is the *only* sanctioned audio-thread allocation; everything the
  framework itself does on that path (payload recycling, per-parameter
  mirror republish without batching) is allocation-free. Plugins can
  still allocate in their own `process` — the trait docs forbid it, but
  Rust cannot enforce it.

## 4. No blocking IPC, filesystem, logging, or UI operation on the audio thread

The audio thread never touches sockets, files, loggers, or webviews.

- **Enforced by:** bus callbacks park values and return (they run on
  bus threads); webview calls are main-thread-only (`Editor::attach`,
  `eval`, `resize`); diagnostics are atomic counters, not log lines.
- **Status: held.**

## 5. Host/UI/IPC parameter changes enter the audio thread through bounded, explicitly realtime-safe mechanisms

Every ingress path — host automation, editor gestures, IPC sync — is a
bounded structure with a documented overflow policy, and none of them
lock the processor.

- **Enforced by:** the host's own event list (CLAP/VST3), and the
  engine's bounded command queues for everything else — one queue for
  ordinary realtime commands (parameter changes, resets; cheap, drained
  in full each block) and a separate one for expensive state commands
  (serviced at most one per block), so a flood of parameter moves can
  neither delay a state op nor extend a callback beyond one bounded
  amount of state work.
- **Status: held.** Host automation is bounded (256 points/block,
  excess counted via `param_events_dropped`), the editor/IPC path is
  the bounded command queue (1024, `commands_dropped` on overflow), and
  the state queue holds at most two commands (capacity 2, and the
  engine's state-op lock keeps at most one round-trip in flight).
  Neither path locks the processor.

## 6. Parameter reads exposed to non-audio threads do not require locking the `Processor`

Hosts and editors read parameter values wait-free, from an atomic
mirror the audio thread publishes.

- **Enforced by:** `ParamMirror` — one `AtomicU64` per parameter plus a
  seqlock generation counter for consistent snapshots.
- **Status: held.** Every host read (`params_get_value`,
  `getParamNormalized`, `AUParameter` getters) and every editor seeding
  goes through the mirror; the processor lock is gone, and
  `EngineHandle::snapshot_params` — which answers `sync_request` in all
  four adapters — reads the mirror rather than the processor.

## 7. MIDI output is audio-thread-owned during processing and transferred without blocking

The DSP's `MidiOut` is written only by `process` and drained into the
host's event output inside the same callback. No other thread ever
touches it.

- **Enforced by:** `MidiOut` ownership inside the engine's audio-side
  core; adapters drain inline after `process` returns.
- **Status: held.** Ownership is structural: `MidiOut` lives in the
  audio-side `AudioCore` and is never behind a lock. (AUv3's shim reads
  the events after the render call returns — legal under the documented
  single-writer contract of `Engine::midi_out`.)

## 8. Every bounded queue has an explicit overflow policy

Capacity is finite, so the full case is named, counted, and documented
— never a silent `break`/`return`/`continue`.

- **Enforced by:** `DiagCounters` (`crates/skuiz-core/src/diag.rs`)
  incremented on every overflow, with the policy written at the point
  of the bound.
- **Status: held.** Every bound counts its drops: `MidiOut` (512),
  param events (256/block, per-parameter overflow drops only that
  point), the command queue (1024), the state-response ring. The
  state-response ring additionally has a *proof* it cannot overflow —
  the state-op lock serializes round-trips, so at most one response is
  ever in flight against a capacity of two — and the audio thread still
  counts (`state_responses_dropped`) and debug-asserts on a push
  failure, so a violation of that proof is caught rather than silent.

## 9. Shared IPC state is eventually convergent; transient message loss cannot leave an instance permanently divergent

A dropped frame, a late joiner, or an election window may delay
convergence but must never prevent it.

- **Enforced by:** versioned last-writer-wins updates (lamport clock +
  origin id, `crates/skuiz-core/src/lww.rs`), a `sync_request` /
  `sync_state` snapshot exchange when an instance joins, and a
  `LINK_UP_FRAME` the bus delivers locally whenever the cross-process
  link (re)connects — answered by a fresh `sync_request`, so frames
  dropped during an election window heal instead of vanishing.
  Crucially, version recording is *atomic with delivery*: `accept_with`
  and `stamp_with` run the engine push while holding the record lock
  and mark a version only if the command actually entered the engine.
  A frame dropped by a full command queue therefore leaves no mark, so
  the identical version re-delivered later still wins; and because the
  lock serializes apply order with version order, concurrent updates
  can never leave the engine holding a stale value under a fresh
  version. The engine side of the guarantee (`Engine::set_param`): a
  *stopped* engine never reports success for a change it only queued —
  the queue moves only while blocks flow, so a stopped-but-contended
  change is retried briefly and then refused, never parked unseen.
  Queued work also stays ordered ahead of direct work: every main-thread
  access drains the queues first, so a change queued in a stop/start
  race heals at the next access instead of stalling until the next run.
- **Status: held.** Stale or reordered frames lose by version; a late
  joiner converges from the snapshot exchange; server death mid-stream
  is survived (traffic resumes after re-election, tested cross-process
  in `crates/skuiz-ipc/src/lib.rs`). The full-queue regression —
  dropped frame, no mark, re-delivery heals — and the stopped-state
  regression — contended change refused, nothing claimed, mirror
  untouched — are tested in `crates/skuiz-core/src/engine.rs`. Legacy
  unversioned frames still apply but never displace versioned state
  permanently — the next sync round heals them.

## 10. Host automation and shared-state synchronization have explicitly defined semantics and are not accidentally conflated

Automation is per-instance and sample-accurate; IPC sync is
cross-instance, block-timed, and restricted to parameters declared
`shared`. Neither path silently borrows the other's behavior.

- **Enforced by:** `ParamDef::shared` — only editor-originated changes
  to shared parameters publish to the bus, and receivers ignore frames
  naming local parameters; host automation and project state loads
  never cross the bus. For sync answers specifically, `Lww` tracks
  per-parameter *freshness*: a parameter is advertised only while the
  value in force is the one its recorded version refers to. A
  `load_state` stales every record (`Lww::on_state_load`) — the
  win/lose memory is kept so stale frames still lose, but the loaded
  values stay local until a shared edit lands after the load and
  re-claims the parameter.
- **Status: held, with one documented gap.** `shared` is a declared
  property of every parameter, all four adapters filter both broadcast
  and receive through it, tests prove local parameters ignore bus
  frames, and a project load's values no longer leak through
  `sync_state` under a stale version
  (`crates/skuiz-clap/tests/ipc_sync.rs`). The gap: while the transport
  runs, *host automation* moves the mirror without touching the LWW
  record, so a `sync_state` answer in that window can still pair an
  automation value with the last bus version. Closing it needs an
  RT-safe taint flag in `ParamMirror`; until then the bound is that
  only instances whose record is older than that version will accept
  the value.

## Changing an invariant

Weakening one of these is a design change, not a code change: it
requires updating this page, `docs/concepts/threading.md`, and the
README in the same commit, and a stated reason in the commit message.
