# AUv3 packaging scaffold

What is here, and what is honestly still missing.

## Present and working

- `crates/skuiz-auv3/src` — the Rust side: `export_auv3!` generates the
  `skuiz_auv3_*` C ABI covering lifecycle, rendering, parameters (including
  choice labels), state, and generated MIDI.
- `crates/skuiz-auv3/shim` — `SkuizAudioUnit`, the `AUAudioUnit` subclass
  that forwards every call to that ABI. Compiled automatically on Apple
  targets, and **run** by the Rust test suite: an `AUAudioUnit` subclass can
  be instantiated and rendered in-process with no bundle, host or signing,
  so `skuiz_auv3_selftest` exercises parameters, rendering with and without
  an input, MIDI output and `fullState` round-tripping.
- `Info.plist` — extension bundle template with the `AudioComponents` entry.
- `Skuiz.entitlements` — App Group entitlements, for cross-host IPC.

## Not here

The Xcode project, provisioning and signing. Those need Xcode and a
developer account, so they are yours to set up; nothing about them can be
built or verified in this repo.

## Wiring it up

1. Build the plugin crate (the one calling `export_auv3!`) as a `staticlib`
   for the Apple targets, and link it plus `skuiz-auv3` into an Audio Unit
   app extension target.
2. Add `shim/SkuizAudioUnit.m` to the extension target, or link the static
   library `cc` produces for it. Set the Info.plist
   `NSExtensionPrincipalClass` to `SkuizAudioUnit`, replacing the
   `$(PRODUCT_MODULE_NAME).SkuizAudioUnitViewController` placeholder if you
   do not add a view controller of your own. Note the scaffold's extension
   point is `com.apple.AudioUnit-UI` (Info.plist), which pairs with a view
   controller; making the `AUAudioUnit` subclass itself the principal class
   may need the plain audio-unit extension point instead — verify against
   the host you target, as nothing in this repo can.
3. Only if you want instances in *different hosts* to sync: set
   `SkuizAudioUnit.skuizAppGroupDirectory` before the unit is instantiated
   (a `+load` method or another early initialiser is the reliable place;
   the value is read in `initWithComponentDescription`), to the path from
   `FileManager.default.containerURL(forSecurityApplicationGroupIdentifier:)`,
   and give the extension and its containing app the same App Group.
   Instances within one host share a process and sync without any of this.
4. Fill in `${VENDOR_CODE}` and `${SUBTYPE_CODE}` in the Info.plist. Both are
   exactly four characters, and the subtype must be unique per plugin.

Run `cargo test -p skuiz-auv3` first: if `objc_shim_renders_through_rust`
passes, the audio path is sound and anything still broken is packaging.

## Process model and sandbox notes

- **All instances of one plugin in one host share a single extension
  process** (confirmed by Apple, developer forums thread 65909; there is no
  way to force separate processes). Skuiz's bus detects this and delivers
  between them by direct call, with no socket involved, so no App Group is
  needed and sync survives even a sandbox denial on the socket path.
  Separate processes — and the App Group — only matter across hosts.
- A crash in any instance kills that shared process and every sibling
  instance with it. The host survives. Nothing Skuiz does changes this;
  keep `process` free of panics.
- Unix sockets in an App Group container are Apple's sanctioned route
  between same-team sandboxed processes. System V shared memory is
  explicitly discouraged under App Sandbox, which is why Skuiz does not use
  a shared-memory IPC library.
- Extensions can be denied *write* access in a group container even with
  entitlements set; if socket creation fails, check the container path and
  that both targets carry the same group.
