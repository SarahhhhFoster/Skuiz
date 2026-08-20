# Instance sync

The feature Skuiz exists for: instances of your plugin find each other
and share state — two instances in one DAW, or one in REAPER and one in
a standalone app on the same machine. You saw it in
[getting started](../getting-started.md#6-try-instance-sync): load two
instances, move a slider, both follow. This page is how.

## Two tiers, one API

Plugin instances usually share a process — a CLAP or VST3 plugin is a
library inside the DAW, and AUv3 instances in one host share an
extension process. So delivery has two tiers:

- **In-process**: instances in the same process reach each other by
  direct callback. No socket is involved.
- **Cross-process**: exactly **one socket per process** (not per
  instance) carries traffic to instances elsewhere. A server relays
  each frame to every node except its sender.

`Bus::send(&bytes)` uses both transparently.

## The Bus API

```rust
let bus = skuiz_ipc::Bus::join(scope, move |frame: &[u8]| { /* ... */ });
bus.send(b"set_param 0 0.5");
bus.is_server(); // exactly one instance machine-wide says true
// Dropping `bus` leaves the bus.
```

`scope` namespaces the bus — the adapters join under your
`PluginInfo::id`, so two *different* plugins never hear each other. For
sandboxed hosts (an App Group container on Apple platforms) there is
`Bus::join_in(dir, scope, callback)`, which points the socket at a
directory the sandbox allows.

The callback runs on a bus thread and must never touch plugin memory —
park the value and let the audio thread apply it. The adapters already
do this for `set_param` messages; you only touch the Bus directly to
send message types of your own. Use `skuiz_core::protocol`'s format for
anything parameter-shaped so editors and the bus stay compatible.

## Which parameters sync

Only parameters declared `shared: true` in their `ParamDef`. The rule
applies in both directions: an editor move on a shared parameter is
broadcast, and a bus frame naming a local parameter is ignored by
receivers rather than applied. Host automation and project state loads
never cross the bus for any parameter — automation is per-instance and
sample-accurate, sync is cross-instance and block-timed, and neither
borrows the other's behavior.

## Convergence: versions and snapshots

Sync is *eventually convergent*, not just fire-and-forget. Every
`set_param` frame an adapter broadcasts carries a version — a lamport
sequence number plus the sender's origin id
(`skuiz_core::lww`). A receiver applies a frame only when its version
beats the last one seen for that parameter, so duplicated, delayed or
reordered frames can slow convergence but never prevent it; ties
resolve deterministically on origin id.

Two mechanisms close the gaps versions alone cannot:

- **Join snapshot.** A new instance broadcasts
  `sync_request <origin>` right after joining. Every instance answers
  with a `sync_state` frame listing the shared parameters it holds a
  version for — ones actually edited over the bus — and
  last-writer-wins makes duplicate answers safe. Parameters never
  edited over the bus are *omitted* from the answer: their value may be
  host automation, which is per-instance and must not propagate to
  joiners, so a joiner's untouched defaults can never drag the fleet
  back.
- **Link-up healing.** Frames sent while the cross-process link is down
  (an election window) are dropped — but when the link (re)connects,
  the bus delivers a synthetic `LINK_UP_FRAME` to local instances and
  each answers with a fresh `sync_request`, pulling the fleet back into
  agreement.

Legacy 3-token `set_param` frames (hand-rolled senders, old peers)
still apply — but they carry no version and leave no mark, so they can
never permanently displace versioned state: the next sync round heals.

## Election and promotion

Exactly one instance machine-wide reports `is_server()`. It should own
writing shared state on save — that is the point of the role.

- On **Unix** (macOS, Linux), the owning *process* wins an `flock` on a
  lock file. The kernel releases the lock if the owner dies, so it can
  never go stale.
- On **Windows**, creating the named pipe with
  `FILE_FLAG_FIRST_PIPE_INSTANCE` *is* the election — the name is a
  kernel object that dies with its owner.
- Within the owning process, the longest-lived instance is the server.

Promotion is automatic: when the server instance is deleted, the next
longest-lived in the process takes over immediately; when the owning
*process* dies, a peer claims the election within about one poll cycle
(~20 ms). Delete the first-loaded instance in your DAW and sync just
keeps working.

## Failure design

If the socket cannot be created at all — a misconfigured App Group, an
unwritable directory — in-process delivery is unaffected, because it
never touches the socket. A sandbox denial costs you cross-host sync,
not everything. One caveat: a very long socket directory can exceed the
Unix socket path limit and silently degrade the same way.

Other honest limitations: the Windows named-pipe transport is
type-checked (and exercised in CI) but has had little real-machine
verification; there is no ordering guarantee between the local and
remote delivery tiers; and the lock file is owned by the first user on
a machine, so cross-*user* sync on a shared machine does not work.
Frames are length-prefixed (4-byte LE) and capped at 1 MiB.

## Doing more than parameters

Parameters sync for free — the adapters forward them. For your own
messages (a shared pattern sequencer's steps, say), join the same scope
and `send` whatever bytes you like; keep frames small and infrequent,
and remember the callback's threading rule above. The server role is
yours to use for anything that needs a single writer.
