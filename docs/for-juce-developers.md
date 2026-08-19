# For JUCE developers

Skuiz owes JUCE the idea that one C++-adjacent codebase should ship
every plugin format — and that is where the resemblance ends. The
design is inspired-by but architecturally distinct (see `PLAN.md`), and
no JUCE code is used: Skuiz is MIT with a hard licensing-purity rule.

## The mapping

| JUCE | Skuiz |
| --- | --- |
| `AudioProcessor` | The `Processor` trait ([processor](concepts/processor.md)) |
| `AudioProcessorValueTreeState` | `params()` + `set_param`/`get_param`, with default `save_state`/`load_state` |
| `AudioProcessorEditor`, `Component` tree | `editor_html()` — a web page in a system webview; there is no component or graphics API ([editors](concepts/editors.md)) |
| `AudioProcessorParameter` / `AudioParameterChoice` | `ParamDef`; a non-empty `choices` list *is* the choice parameter ([parameters](concepts/parameters.md)) |
| `MessageManager`, `AsyncUpdater` | The bus callback parks values; the audio thread drains them at block top ([threading](concepts/threading.md)) |
| `juce::dsp` | Your own Rust, C over FFI, or embedded Pure Data ([DSP](guides/dsp.md)) |
| Projucer / CMake | cargo + `crate-type = ["cdylib"]` + small bundle scripts ([packaging](guides/packaging.md)) |
| `InterprocessConnection` for instances | Nothing in JUCE does this — it's what `skuiz-ipc` adds ([instance sync](concepts/instance-sync.md)) |

## The three biggest adjustments

**The editor is a document, not a view hierarchy.** You will not lay
out components or paint in `paint()`. You write HTML/CSS/JS and
exchange `set_param` strings with Rust. This is lighter than JUCE's
editor model and ports to every format for free, but if your GUI is a
custom-rendered spectrum display, you are writing canvas/WebGL code,
not `Graphics` calls.

**State is parameters by default.** JUCE encourages a value tree with
arbitrary attachments; Skuiz's default state is exactly the parameter
list. Extra state means overriding `save_state`/`load_state` — fine,
but it is the exception path, not the default architecture.

**Instances are peers.** In JUCE, two instances are two unrelated
objects unless you build the bridge yourself. In Skuiz they discover
each other and elect an owner out of the box, in-process and across
processes. Features that are painful in JUCE — a shared preset pool,
one instance controlling others — are the native case here.

## Licensing contrast

JUCE is GPL/commercial; Skuiz is MIT. The one obligation that survives
the move is Steinberg's: shipping a **VST3 binary** still requires
their license (GPLv3 or the free proprietary agreement), which is why
the VST3 adapter is opt-in. CLAP and standalone carry nothing. See
[VST3 licensing](formats/vst3.md#licensing).
