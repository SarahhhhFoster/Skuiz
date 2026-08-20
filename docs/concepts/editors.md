# Editors

A Skuiz editor is a web page. There is no widget tree or drawing API:
`editor_html()` returns a static HTML string, `editor_size()` its
logical-pixel size, and the adapter embeds it in the host's window with
the system webview (wry: WebKit on macOS, WebView2 on Windows).

```rust
fn editor_html() -> Option<&'static str> {
    Some(include_str!("editor.html")) // compiled in — no asset to install
}

fn editor_size() -> (u32, u32) {
    (320, 120)
}
```

One HTML document drives every format — CLAP's `gui` extension, VST3's
`IPlugView`, and the standalone window all attach the same page.

## The bridge is two functions

**Page → plugin:** `window.ipc.postMessage("set_param 0 0.75")` — a
plain string, parsed by `skuiz_core::protocol::parse_set_param`.

**Plugin → page:** the adapter evaluates
`window.skuizOnParam(id, value)` in the page. Define it, and guard the
definition order — the adapter always calls it as
`window.skuizOnParam && window.skuizOnParam(...)` so an early call
before your script runs is dropped, not an error.

That is the entire protocol. Everything else — sliders, frameworks,
visualizations — is ordinary web development.

There is one more message kind, for diagnostics: the page posts
`"skuiz_diag"` and the plugin answers with
`window.skuizOnDiag && window.skuizOnDiag({...})`, a plain object of the
instance's drop counters (`param_events_dropped`, `midi_events_dropped`,
`commands_dropped`, `bus_frames_dropped`, `mirror_retries` — see
`skuiz_core::diag`). Wired in the CLAP, VST3 and standalone editors;
AUv3 has no webview editor. Useful for a debug overlay, ignorable
otherwise.

## Two rules that keep editors sane

**Push your state on mount.** The adapter seeds the page with current
values right after attaching, but that eval can race your script
loading, and the guarded call drops it silently. The reliable
direction is the page pushing: on mount, post every parameter value
your UI is showing. If the host has a different value, the answer
comes back through `skuizOnParam`.

**Never echo host values back out.** When `skuizOnParam` delivers a
value (host automation, a preset load, another instance), update the
widget — but do not let that update re-post `set_param`. Two editors
each echoing the other is an infinite loop. Track which values are
"agreed" and suppress the echo; `examples/solid-synth`'s editor does
this with an `agreed` map, and `verify-editor.mjs` asserts it
headlessly.

## Using a framework

The page is a plain document — use any framework or none.
`examples/solid-synth` uses SolidJS, vendored as a prebuilt ~31 KB
bundle and `concat!`ed into the HTML, so building the plugin needs
cargo and no JavaScript toolchain. Its signals hold the parameters and
a `createEffect` per signal posts changes — that pattern is worth
reading before writing your own reactive editor.

You can test editor logic without a plugin host: render the page in
jsdom and assert on the messages. See
`examples/solid-synth/verify-editor.mjs`, which CI runs.

## Platform support

macOS is tested. Windows (`from_hwnd`) and Linux (`from_x11`, X11 via
WebKitGTK — on Wayland, through the host's X11-embedding support) are
written but unverified — they type-check and ship in CI but have had no
real-world exercise. See
[platform support](../reference/platform-support.md).

## Automation recording caveat

Editor moves reach the host as a parameter rescan, which syncs values
but does not record an automation *gesture* in every host. VST3 is the
exception: the adapter wraps editor changes in
`beginEdit`/`performEdit`/`endEdit`, which hosts record properly. If
CLAP automation recording from the GUI matters to you, that is a known
ponytail in the adapter.
