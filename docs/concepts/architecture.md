# Architecture

Skuiz is built around one idea: **your plugin is a plain Rust type, and
everything format-specific is a translation layer you never touch.**

## The shape of a Skuiz plugin

```
                    your type: impl Processor
                              │
                    ┌─────────┴─────────┐
                    │   skuiz-core      │   the trait, parameters, MIDI
                    │  (no format code) │   buffer, wire protocol
                    └─────────┬─────────┘
                              │
      ┌───────────┬───────────┼───────────┬────────────┐
      │           │           │           │            │
 skuiz-clap  skuiz-vst3  skuiz-auv3  skuiz-standalone  (future formats)
      │           │           │           │
   CLAP host   VST3 host   AUv3 host   desktop app
```

An adapter's whole job is translating one host's conventions into calls on
your `Processor`. It is the only code that knows what a `clap_process` or an
`AUAudioUnit` is. Adding a format means writing a new adapter, not touching
plugins.

Alongside that spine sit services an adapter wires in for you:

```
   skuiz-ui     webview editor, attached to the host's native window
   skuiz-ipc    instance-to-instance messaging and owner election
   skuiz-midi   MIDI 1.0 construction and output configuration
   skuiz-dsp    embedded Pure Data (optional)
```

## The crates

| Crate | Purpose | You use it |
| --- | --- | --- |
| `skuiz-core` | The `Processor` trait, `ParamDef`, `MidiOut`, wire protocol | Always |
| `skuiz-clap` | CLAP adapter and `export_clap!` | To ship CLAP |
| `skuiz-vst3` | VST3 adapter and `export_vst3!` | To ship VST3 |
| `skuiz-auv3` | AUv3 C ABI and `AUAudioUnit` shim | To ship AUv3 |
| `skuiz-standalone` | Window, audio device, and editor for a desktop app | For a desktop build |
| `skuiz-ui` | Webview embedding | Rarely — adapters drive it |
| `skuiz-ipc` | The instance bus | Rarely — adapters drive it |
| `skuiz-midi` | MIDI messages, channel dropdown | If you emit MIDI |
| `skuiz-dsp` | Embedded Pure Data | If you embed Pd |

Most plugins depend on exactly two: `skuiz-core` and one adapter.

## Three things that surprise people

### One binary can be several formats

`export_clap!` and `export_vst3!` define different entry-point symbols, so
both can live in one `cdylib`. The `.clap` and `.vst3` bundles then wrap the
*same* compiled library — only the surrounding directory differs. See
[packaging](../guides/packaging.md).

### The editor is a web page, not a widget tree

There is no component or drawing API. `editor_html()` returns a string, the
adapter hands it to a system webview, and two functions carry messages
across. Your UI is HTML, CSS, and whatever JavaScript you like — including a
reactive framework. See [editors](editors.md).

### Instances are not isolated

In most plugin frameworks, two instances of your plugin are two unrelated
objects. In Skuiz they are peers on a bus: they discover each other, elect
one owner, and exchange messages, whether they share a process or not. This
is the capability Skuiz exists to provide. See
[instance sync](instance-sync.md).

## Where the boundaries are

Understanding what each layer guarantees makes debugging much faster.

**Your `Processor`** owns all plugin state. It never learns which format is
hosting it, so the processor implementation itself is identical for a CLAP
validator and a VST3 host. (The *export macros* may sit behind cargo
features — that's packaging, not conditionals in your DSP.)

**The adapter** owns the host conversation: buses, parameter reporting,
state serialization, the editor window, and joining the bus. It also owns
the threading discipline — it decides what reaches your `process` and when.
See [threading](threading.md).

**`skuiz-core`** owns the contracts both sides agree on: what a parameter
is, how state is serialized, and the `set_param <id> <value>` message format
that editors and the bus both speak.

The practical consequence: if a plugin misbehaves in one host but not
another, the bug is almost certainly in an adapter, not in your DSP — and
the CLAP validator will usually name it.
