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

## The built-in bridge

**Page → plugin:** `window.ipc.postMessage("set_param 0 0.75")` — a
plain string, parsed by `skuiz_core::protocol::parse_set_param`.

**Plugin → page:** the adapter evaluates
`window.skuizOnParam(id, value)` in the page. Define it, and guard the
definition order — the adapter always calls it as
`window.skuizOnParam && window.skuizOnParam(...)` so an early call
before your script runs is dropped, not an error.

For diagnostics, the page posts `"skuiz_diag"` and the plugin answers
with `window.skuizOnDiag && window.skuizOnDiag({...})`, a plain object
of the instance's drop counters (`param_events_dropped`,
`midi_events_dropped`, `midi_in_events_dropped`,
`editor_messages_dropped`, `commands_dropped`, `bus_frames_dropped`,
`state_responses_dropped`, `mirror_retries` — see `skuiz_core::diag`).
Wired in the CLAP, VST3 and standalone editors; AUv3 has no webview
editor. Useful for a debug overlay, ignorable otherwise.

## Custom messages

Anything outside `set_param` and `skuiz_diag` is a custom message. This
covers UI events that are not parameter state: audition a chord, assign a
grid slot, flash a control when MIDI arrives, and so on.

**Page → processor:** post a plain string, and implement
`Processor::on_editor_message`:

```js
window.ipc.postMessage('audition 42');
```

**Processor → page:** store the `EditorSender` the engine supplies just
after construction, send it a string, and implement
`window.skuizOnMessage`:

```rust
use skuiz_core::engine::EditorSender;

struct MyProcessor {
    editor: Option<EditorSender>,
    // ...
}

impl Processor for MyProcessor {
    fn set_editor_sender(&mut self, sender: EditorSender) {
        self.editor = Some(sender);
    }

    fn on_editor_message(&mut self, message: &str) {
        if message == "ping" {
            if let Some(editor) = &self.editor {
                editor.send("pong");
            }
        }
    }
    // ...
}
```

```js
window.skuizOnMessage = (text) => {
  if (text === 'pong') { /* update the page */ }
};
```

The adapter evals a guarded, JSON-escaped
`window.skuizOnMessage && window.skuizOnMessage(...)`, so arbitrary text
cannot break the JavaScript call and an early call is dropped safely.

### Threading and limits

Incoming custom messages call `on_editor_message` directly on the main
thread when stopped. While blocks flow they travel through a bounded
queue and run on the **audio thread at block top**, which is what makes
them suitable for triggering MIDI. Keep the handler to cheap parsing and
assignment, with the same no-allocation/no-locking rules as `set_param`.

Outgoing `EditorSender::send` is allocation-free and realtime-safe. The
adapter drains it on the main thread: CLAP uses `request_callback`, VST3
uses its `getParamNormalized` polling point, and standalone posts a UI
event. Do not call the sender from bus callbacks or arbitrary threads.

Messages are at most **256 UTF-8 bytes**; each direction queues **64**
messages. Overlong messages and full queues return `false` and increment
`editor_messages_dropped`. Parameter messages and custom messages use
separate queues, so their relative order is not guaranteed.

Custom output is **transient, not a state-synchronization mechanism**:
messages emitted while no editor is open are discarded, and calls made
before the page installs its handler are dropped. Push durable state on
mount, or represent it as parameters/custom state and have the page
request it. AUv3 has no webview editor channel.

Everything else — sliders, frameworks, visualizations — is ordinary web
development.

## Two rules that keep editors sane

**Push your state on mount.** The adapter seeds the page with current
values right after attaching, but that eval can race your script
loading, and the guarded call drops it silently. The reliable
direction is the page pushing: on mount, post every parameter value
your UI is showing. On CLAP with the transport running, the applied
value comes back through `skuizOnParam` once the engine has taken it;
on VST3 and the standalone there is no echo, so treat your own push as
authoritative.

**Never echo incoming values back out.** When `skuizOnParam` delivers a
value (a preset load, another instance, or the CLAP echo of your own
push), update the widget — but do not let that update re-post
`set_param`. Two editors
each echoing the other is an infinite loop. Track which values are
"agreed" and suppress the echo; `examples/solid-synth`'s editor does
this with an `agreed` map, and `verify-editor.mjs` asserts it
headlessly.

## Using a framework

The page is a plain document — use any framework or none.
`examples/solid-synth` uses SolidJS plus solid-knobs, vendored as
prebuilt bundles (~63 KB total) and `concat!`ed into the HTML, so
building the plugin needs cargo and no JavaScript toolchain. Its signals hold the parameters and
a `createEffect` per signal posts changes — that pattern is worth
reading before writing your own reactive editor.

You can test editor logic without a plugin host: render the page in
jsdom and assert on the messages. See
`examples/solid-synth/verify-editor.mjs`, which CI runs.

## Platform support

macOS is tested. Windows (`from_hwnd`) and Linux (`from_x11`, X11 via
WebKitGTK — on Wayland, through the host's X11-embedding support) are
written but unverified — they type-check in CI but have had no
real-world exercise, and the release workflow ships macOS and Linux
bundles only, nothing for Windows yet. See
[platform support](../reference/platform-support.md).

## Automation recording caveat

Editor moves reach the host as a parameter rescan, which syncs values
but does not record an automation *gesture* in every host. VST3 is the
exception: the adapter wraps editor changes in
`beginEdit`/`performEdit`/`endEdit`, which hosts record properly. If
CLAP automation recording from the GUI matters to you, that is a known
ponytail in the adapter.
