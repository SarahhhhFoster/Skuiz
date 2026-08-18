# SKUIZ
## Intent
The intent behind Skuiz is to make a cross-platform library on which to build plugin/DSP projects which provide IPC endpoints for communication between plugin instances, with intent to target standalone executables as well as VST3, AUv3, and CLAP plugins.
## Architecture
Skuiz will include four essential components: a UI library, DSP processing, IPC communications layer, and an optionally-included plugin I/O layer with the relevant adapters for the aforementioned plugin formats.
### UI Library
For UI, Skuiz aims to use something with which many authors will be familiar: an embedded web view. Instead of something heavier like Electron, we're going to use [Tauri](https://tauri.app/), and, where possible, we'd like to avoid loading libraries for each plugin. We want to be memory-conscious. The essential goal, though, is to work across platforms and be efficient. React should be supported, but our provided examples will use Lit and Stencil for efficiency.
### DSP/DAW Control
For DSP, I would like to have examples using quick-and-dirty C, something compiled to run on the GPU (perhaps a spectral resynthesizer), and embedded libpd.
### IPC Communication
I'm not attached to a particular IPC strategy, but my initial thought is that we provide a convenience methodology for having the first instance of a particular plugin open an IPC channel with itself acting as a server, with functions for mapping and reducing messages for distribution to clients, along with zero-configuration promotion of clients to servers as plugins are deleted, to ensure shared state. On project save in a DAW, the server should own saving any state that has to be shared between instances.
### Plugin Interface
Something that takes a set of messages sent from our DSP and DAW control and distributes it over available interfaces. I'd like to default to MIDI, but support, where possible, MPE and MIDI 2.0. This should be selectable with a dropdown menu which plugin developers can also use to add further configuration items (bit depth? scale and microtuning? that sort of thing)
## Inspiration
This will be inspired by, but be essentially architecturally distinct from, JUCE. Do not pull from JUCE; keep this pure from a licensing perspective. Also, the way we think of plugins obviously owes a debt to VST from Steinberg, the SDK for which is now [open source](https://github.com/steinbergmedia/vst3sdk), simplifying our development against it.