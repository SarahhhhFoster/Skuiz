# AUv3

AUv3 is the one format Rust cannot ship alone: an app extension needs
an Objective-C `AUAudioUnit` principal class built and signed by Xcode.
So the crate is two halves, and the honest status is: **the Rust side
and the shim are done and tested; the Xcode target is yours to
assemble.**

## What exists today

- **A flat C ABI** — `export_auv3!(MyProcessor)` generates
  `skuiz_auv3_init/destroy/activate/deactivate/render`, parameter
  info/get/set, state save/load, and a MIDI drain over an opaque
  instance pointer. Covered by `cargo test -p skuiz-auv3`.
- **An Objective-C shim** (`shim/SkuizAudioUnit.m`, compiled by
  `build.rs`) that owns no state: it builds an `AUParameterTree` from
  the C ABI (choice parameters become indexed parameters with
  `valueStrings`), pulls input in its render block, and forwards
  generated MIDI with block-relative timestamps. The shim is *executed*
  in tests — a selftest instantiates and renders a real unit
  in-process.
- **A packaging scaffold** (`scaffold/`): an extension `Info.plist` and
  a README walking through the Xcode assembly.

What does not exist: the assembled, signed Xcode target, host
discovery, and provisioning. The scaffold README flags the known
unknowns — including that its principal-class substitution may require
changing the extension point identifier — as needing host verification.

## The App Group timing trap

Cross-host sync on Apple platforms needs the socket inside an App Group
container, so the shim reads `skuizAppGroupDirectory` to decide where
the bus lives. **Set it before the unit is instantiated** — the value
is consumed in `initWithComponentDescription`. Setting it any later is
a silent no-op: the bus binds in the sandbox temp dir and cross-host
sync silently does nothing. A `+load` or other early initializer is
the reliable hook, since hosts instantiate the principal class without
running your app code first.

Within one host this doesn't matter: AUv3 instances share the
extension process, so they sync by direct in-process call regardless —
the two-tier design working as intended. See
[instance sync](../concepts/instance-sync.md).

## State and parameters

State flows through the unit's `fullState` as an `NSData` blob under
`"skuiz.state"`, using the default `save_state`/`load_state` format.
Parameter changes from the host UI or editor use the broadcasting
setter (so other instances follow); scheduled events from the render
thread use a render-safe setter that allocates nothing and broadcasts
nothing.

## Shipping checklist

1. `export_auv3!(MyProcessor);` in a `staticlib`/`cdylib` the shim
   links against.
2. Assemble the app extension in Xcode from `scaffold/` — principal
   class, entitlements (App Group if you want cross-host sync).
3. Sign and provision with an Apple developer account.
4. Test in a real AUv3 host. Everything before this step is covered by
   the in-process tests; this step is not.
