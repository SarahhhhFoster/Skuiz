# Threading and realtime rules

The one correctness page. Read it before shipping anything.

## Which thread calls what

| Method | Thread |
| --- | --- |
| `process` | Audio thread, once per block |
| `set_param` | Audio thread (automation, IPC) **and** main thread (editor, state load) |
| `get_param` | Any thread |
| `activate` / `deactivate` | Main thread |
| `info` / `params` / `editor_html` / `editor_size` | Any thread; expected to be constant |
| `save_state` / `load_state` | Main thread |
| Bus callback (`Bus::join`) | A bus thread — must never touch plugin memory |

## The realtime rules

`process` runs on the host's audio thread, where a missed deadline is
an audible glitch. In `process` — and in anything it calls:

- **No allocation.** Allocate in `activate` using the announced maximum
  block size.
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

Each adapter holds **one `Mutex` around your processor**. The audio
thread takes it for each block; the main thread takes it for state
loads and editor changes. Critical sections are kept short on both
sides, and a poisoned lock is recovered (`into_inner`) rather than
propagated — a panic elsewhere must not cascade into a permanently
silent plugin.

The trade-off is real: a long main-thread hold (say, a slow
`load_state` you overrode) can block the audio thread. Keep
main-thread work under the lock short; do expensive preparation
outside it. Lock-free parameter sync is a documented deferred item
(see the README) — if contention ever becomes audible, that's the
upgrade path, not something you work around in your plugin.

## How IPC values reach the DSP

A message from another instance never touches your processor directly.
The bus callback — running on a bus thread — parses the frame and
parks `(id, value)` in a pending queue. At the top of the next block,
the adapter drains the queue and calls `set_param` for each. This is
why IPC-delivered changes apply at block top (they carry no timing,
unlike host automation, which the adapters apply sample-accurately),
and why a destroyed instance can never be called back: the queue lives
in an `Arc` the callback owns.

## Editors

Webview calls are main-thread-only: `Editor::attach`, `eval`,
`resize`, and the `Editor` drop. Adapters snapshot parameter values
under the processor lock and release it *before* evaluating any
JavaScript, because a script eval is a call of unbounded cost. If you
build editor-adjacent code of your own, keep that ordering: lock,
copy, unlock, then talk to the webview.
