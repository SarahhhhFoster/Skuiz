# Platform support

Development happens on macOS. This page says plainly what is **tested**,
what is only **type-checked**, and what is **not implemented** — the
same labels the rest of the docs use.

## By platform

| | macOS | Windows | Linux |
| --- | --- | --- | --- |
| Audio + parameters + state | tested | type-checked | type-checked |
| Instance bus | tested (Unix socket) | type-checked (named pipe); the Windows CI job is the only place it executes | type-checked (Unix socket) |
| Webview editor | tested | written, unverified | not implemented |
| Standalone shell | tested | type-checked | type-checked |

If you are on Windows, `cargo test -p skuiz-ipc` is the place to start:
it covers election, promotion, broadcast, and a genuine cross-process
exchange. The named-pipe transport's recent changes are reasoned, not
runtime-verified — treat a Windows CI failure as a real finding.

## By format

| Format | Status | Obligation on you |
| --- | --- | --- |
| CLAP | Working, validator-clean | None (MIT) |
| VST3 | Working, with editor; Steinberg-validator-checked in CI | None — SDK is MIT since v3.8 ([details](../formats/vst3.md#licensing)) |
| AUv3 | C ABI + Obj-C shim done and tested; Xcode target not assembled | Apple developer account |
| Standalone | Working (output + test tone) | None |

## Sandboxes (iOS / App Groups)

In-process delivery never touches a socket, so instances inside one
host keep syncing even where the socket cannot be created — a
misconfigured App Group costs cross-host sync, not everything. The AUv3
shim needs the App Group directory set **before instantiation**; see
[the trap](../formats/auv3.md#the-app-group-timing-trap).

## Deferred (add when needed)

- Lock-free parameter sync (currently one Mutex around the processor)
- MPE per-note expression (UMP events and MIDI 2.0 output are in;
  MPE note-expression events are still out)
- MIDI *input* (only output ports exist today)
- GPU spectral resynthesis example
- AUv3 Xcode project, provisioning and signing
- VST3 CC / pitch-bend output (note on/off are converted)
- Standalone input capture and MIDI output
- Linux webview editor (X11); Windows editor verification
- Running the Windows test suite on real Windows hardware
