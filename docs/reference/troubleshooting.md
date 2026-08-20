# Troubleshooting

Symptom → likely cause → where to look.

## The plugin doesn't appear in my host

- The bundle layout is wrong, or `CFBundleExecutable` doesn't match the
  binary name. Compare against `examples/shared-gain/bundle.sh`.
- You built an `rlib`, not a `cdylib` — check `crate-type` in
  `Cargo.toml`.
- Run `clap-validator validate` on the bundle; it names contract
  violations the host hides. See [packaging](../guides/packaging.md).

## The editor is blank or missing

- `editor_html()` returns `None` (the default) — did you override it?
- On Windows and Linux the editor path is written but unverified (Linux
  embeds on X11 via WebKitGTK; on Wayland it depends on the host's
  X11-embedding support).

## The editor opens but controls do nothing

- The page must post the exact string `set_param <id> <value>` via
  `window.ipc.postMessage` — check for typos or extra tokens.
- The page must push its state on mount; the initial seeding eval can
  race page load and is dropped by design. See
  [editors](../concepts/editors.md).

## Two instances don't sync

- Different `PluginInfo::id` = different buses. They must match exactly.
- AUv3 cross-host sync: `skuizAppGroupDirectory` set *after*
  instantiation is a silent no-op — see
  [the trap](../formats/auv3.md#the-app-group-timing-trap).
- A very long socket directory can exceed the Unix socket path limit
  and degrade to in-process-only.

## Sync works inside one DAW but not across two

That is the socket tier failing while the in-process tier works — by
design, the failure is contained. Likely a sandboxed or unwritable
socket directory; use `Bus::join_in` with an allowed directory. On a
shared machine, note the lock file is owned by the first user —
cross-*user* sync does not work.

## Two editors drive each other forever

Your page re-posts values it received through `skuizOnParam`. Values
arriving from the host must update the widget without emitting
`set_param` again — track "agreed" values and suppress the echo, as
`examples/solid-synth` does. See [editors](../concepts/editors.md).

## Audio glitches when a project loads or a preset lands

While the transport runs, `load_state` is routed onto the audio thread
between blocks (the engine's bounded state round-trip), so an override
that does slow work — parsing, allocation — spends the audio thread's
deadline. Do the expensive preparation lazily: parse into prepared
structures once, and keep `load_state` itself to assignment. See
[threading](../concepts/threading.md).

## My automation moves sound stepped

Host automation is sample-accurate — the block is split at event
times — so stepping usually means a control moved a long way in one
gesture and nothing smoothed it. Smooth fast-moving controls in the
DSP (see `examples/solid-synth`'s per-sample smoothing). Note that
editor and IPC-driven changes carry no timing and apply at block top.

## Parameter text looks wrong in the host

Choice parameters report labels for in-range indices and the raw number
for anything else — a number where you expect a label means the value
is out of range, which usually means a clamp is missing in
`set_param`. Param id `u32::MAX` is reserved (CLAP's invalid id) and
refused.

## On Windows specifically

The named-pipe transport is type-checked and CI-run but not verified on
real hardware. Start with `cargo test -p skuiz-ipc` and report what you
find — that suite covers election, promotion, broadcast, and real
cross-process exchange.
