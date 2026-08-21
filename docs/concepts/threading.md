# Threading and realtime rules

The one correctness page. Read it before shipping anything. The binding
contract lives in [invariants](invariants.md); this page explains the
mechanics.

## Which thread calls what

| Method | Thread |
| --- | --- |
| `process` | Audio thread, once per block |
| `set_param` | Audio thread while blocks flow; main thread when stopped |
| `get_param` | Same as `set_param` — while blocks flow, non-audio threads read the parameter mirror instead |
| `activate` / `deactivate` | Main thread |
| `info` / `params` / `editor_html` / `editor_size` | Any thread; expected to be constant |
| `save_state` / `load_state` | Main thread when stopped; routed onto the audio thread between blocks while running |
| `reset` | Audio thread between blocks while running; main thread when stopped |
| `latency` | Audio thread (polled once per block) and main thread — must be realtime-safe |
| Bus callback (`Bus::join`) | A bus thread (or the sending thread, for in-process delivery) — must never touch plugin memory |

## The realtime rules

`process` runs on the host's audio thread, where a missed deadline is
an audible glitch. In `process` — and in anything it calls:

- **No allocation.** Allocate in `activate` using the announced maximum
  block size. The one documented exception is host-initiated state
  (de)serialization: while the transport runs, `save_state`/`load_state`
  execute on the audio thread between blocks (that is where the
  processor lives), and the plugin's `save_state` may allocate. The
  framework around it does not — payload buffers are recycled back to
  the main thread, and the post-load mirror republish walks the
  parameter list without building anything.
- **No locking.** No mutexes, no channels, no `RwLock`.
- **No I/O.** No files, no sockets, no logging (most loggers lock and
  allocate).
- **No panicking.** The adapters contain panics at the FFI boundary
  with `catch_unwind` — that is a safety net that keeps the host alive,
  not a license. A panicked block renders silence and your plugin is in
  an unknown state.

`set_param` runs on the audio thread too, so the same rules apply to
it in miniature: arithmetic and assignment only.

## How the adapters are built (and what it means for you)

Every adapter wraps your processor in an `Engine`
(`crates/skuiz-core/src/engine.rs`). The engine is a three-state access
machine — idle, main, audio — over the processor, so exactly one thread
owns it at any moment and **no mutex ever appears on the audio
thread**:

- **Audio thread.** Claims the audio state when processing starts (the
  adapters do this in `start_processing`/`setProcessing`, or around each
  render call on AUv3 and the standalone, which have no such pair) and
  runs `process` directly. The claim is structural: `begin_audio` returns an
  `AudioToken`, and every audio-side entry point (`audio_core`,
  `midi_out`) requires a borrow of it — safe code cannot reach the
  processor without one. While blocks flow, nothing else can touch the
  processor.
- **Main thread.** `activate`, `deactivate`, and state save/load take
  the main state — but only while the transport is stopped. While
  running, state save/load becomes a bounded round-trip on a *separate*
  state-command queue (a mutex on the main side serializes round-trips,
  so at most one is ever in flight), the audio thread services at most
  one state op per block, and the main thread waits (bounded, 2 s) for
  the answer. A flood of parameter commands can neither delay a state
  op nor occupy its queue.
- **Everyone else** — editor callbacks, the IPC bus thread — never
  touches the processor at all. Changes go into a bounded command queue
  the audio thread drains at block top; reads are answered wait-free by
  the **parameter mirror** (one `AtomicU64` per parameter plus a seqlock
  for consistent snapshots), which the audio thread publishes as it
  applies changes.

Every queue is bounded and every overflow is counted in
`skuiz_core::diag::DiagCounters` — never silently dropped. If a counter
moves, something pathological happened (a host flooding thousands of
parameter changes per second); the counter is how you find out.

## How IPC values reach the DSP

A message from another instance never touches your processor directly.
The bus callback — running on a bus thread, or synchronously on the
sending thread when the peer lives in the same process — parses the
frame and pushes
`set_param` onto the command queue through an `EngineHandle`. At the top
of the next block, the audio thread drains the queue and calls
`set_param` for each. This is why IPC-delivered changes apply at block
top (they carry no timing, unlike host automation, which the adapters
apply sample-accurately), and why a destroyed instance can never be
called back: the handle holds a `Weak` to the engine, so a frame in
flight after destroy simply goes nowhere.

## Editors

Webview calls are main-thread-only: `Editor::attach`, `eval`,
`resize`, and the `Editor` drop. Adapters seed a freshly opened editor
from the parameter mirror — wait-free, so there is no lock to hold
across a script eval of unbounded cost — and push later changes the same
way. If you build editor-adjacent code of your own, follow the same
rule: read the mirror, then talk to the webview, never the other way
round.
