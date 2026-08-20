//! skuiz-core: the format-agnostic pivot abstraction.
//!
//! Plugin authors implement [`Processor`]; format adapter crates
//! (skuiz-clap, skuiz-vst3, ...) translate host callbacks into calls on it.

#![warn(missing_docs)]
/// Static metadata identifying a plugin to hosts.
pub struct PluginInfo {
    /// Reverse-DNS unique id, e.g. `"com.example.shared-gain"`.
    ///
    /// This is the plugin's identity everywhere it matters, so it must never
    /// change once released: hosts key saved projects on it, the VST3 class
    /// id is derived from it, and instances find each other on the IPC bus
    /// by it. Two plugins sharing an id would share a bus.
    pub id: &'static str,
    /// Display name shown in the host's plugin list.
    pub name: &'static str,
    /// Vendor or author name, as hosts group plugins by it.
    pub vendor: &'static str,
    /// Version string, conventionally semver (`env!("CARGO_PKG_VERSION")`).
    pub version: &'static str,
    /// One-line description for hosts that show one.
    pub description: &'static str,
}

/// A single automatable parameter.
///
/// A parameter with a non-empty `choices` list is a discrete config item:
/// hosts show it as a stepped enum and Skuiz editors render it as a
/// dropdown. This is the mechanism behind PLAN.md's configuration menu —
/// output interface, bit depth, tuning and so on are all just choice
/// parameters, so they automate, save, and sync over IPC like anything else.
pub struct ParamDef {
    /// Stable id, also the key in saved state and IPC messages. Like
    /// [`PluginInfo::id`], changing it breaks saved projects.
    pub id: u32,
    /// Display name shown by the host and by editors.
    pub name: &'static str,
    /// Lowest value of a continuous range. Ignored when `choices` is
    /// non-empty; use [`ParamDef::low`] to read the effective minimum.
    pub min: f64,
    /// Highest value of a continuous range. Ignored when `choices` is
    /// non-empty; use [`ParamDef::high`] to read the effective maximum.
    pub max: f64,
    /// Default value; an index into `choices` for choice parameters.
    pub default: f64,
    /// Labels for a discrete parameter, or `&[]` for a continuous one.
    pub choices: &'static [&'static str],
}

impl ParamDef {
    /// Lowest legal value (always 0 for choice parameters).
    pub fn low(&self) -> f64 {
        if self.choices.is_empty() {
            self.min
        } else {
            0.0
        }
    }

    /// Highest legal value (last index for choice parameters).
    pub fn high(&self) -> f64 {
        if self.choices.is_empty() {
            self.max
        } else {
            (self.choices.len() - 1) as f64
        }
    }

    /// Label for `value`, if this is a choice parameter and `value` is in
    /// range. Out-of-range values give `None` rather than the nearest label,
    /// so a wrong value shows up as a number instead of a plausible lie.
    pub fn label(&self, value: f64) -> Option<&'static str> {
        let idx = value.round();
        if idx < 0.0 {
            return None;
        }
        self.choices.get(idx as usize).copied()
    }
}

/// One generated MIDI event: up to four 32-bit UMP words, so a MIDI 1.0
/// channel-voice message (one word, message type 0x2) and any MIDI 2.0
/// message fit the same slot. Build these with the `skuiz-midi`
/// constructors rather than by hand.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MidiEvent {
    words: [u32; 4],
    len: u8,
}

impl MidiEvent {
    /// Wrap 3 MIDI 1.0 bytes as one UMP word (message type 0x2, group 0).
    /// Adapters still hand these to the host as native MIDI 1.0 events —
    /// the UMP form is the transport inside Skuiz, not necessarily on the
    /// wire.
    pub fn from_midi1(bytes: [u8; 3]) -> Self {
        Self {
            words: [
                0x2000_0000 | (bytes[0] as u32) << 16 | (bytes[1] as u32) << 8 | bytes[2] as u32,
                0,
                0,
                0,
            ],
            len: 1,
        }
    }

    /// Wrap raw UMP words. Anything past four words is dropped: no channel
    /// message is longer, and longer stream messages (sysex) need a
    /// different path anyway.
    pub fn from_ump(words: &[u32]) -> Self {
        let len = words.len().min(4) as u8;
        let mut out = [0; 4];
        out[..len as usize].copy_from_slice(&words[..len as usize]);
        Self { words: out, len }
    }

    /// The valid UMP words.
    pub fn words(&self) -> &[u32] {
        &self.words[..self.len as usize]
    }

    /// The 3 MIDI 1.0 bytes, if this event is a MIDI 1.0 channel-voice
    /// message; `None` for anything wider (MIDI 2.0).
    pub fn midi1_bytes(&self) -> Option<[u8; 3]> {
        if self.len == 1 && self.words[0] >> 28 == 0x2 {
            let w = self.words[0];
            Some([
                ((w >> 16) & 0xFF) as u8,
                ((w >> 8) & 0xFF) as u8,
                (w & 0xFF) as u8,
            ])
        } else {
            None
        }
    }
}

/// MIDI emitted during one processing block, with the frame offset each
/// event lands on. Events are UMP words (see [`MidiEvent`]): MIDI 1.0 and
/// MIDI 2.0 both fit. Adapters drain this into the host's event output.
/// Fixed capacity: [`MidiOut::push`] never allocates, and silently drops
/// events once full — a full buffer means the DSP is emitting thousands of
/// events per block, which is a bug in the DSP, not here.
pub struct MidiOut {
    events: Vec<(u32, MidiEvent)>,
}

impl MidiOut {
    /// Allocate room for `capacity` events. Call off the audio thread.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(capacity),
        }
    }

    /// Queue `event` at `frame` within the current block. Realtime-safe.
    pub fn push(&mut self, frame: u32, event: MidiEvent) {
        if self.events.len() < self.events.capacity() {
            self.events.push((frame, event));
        }
    }

    /// Every queued event as `(frame_offset, event)`, in push order.
    /// Adapters drain this after [`Processor::process`] returns.
    pub fn events(&self) -> &[(u32, MidiEvent)] {
        &self.events
    }

    /// Drop all queued events, keeping the allocation. Adapters call this
    /// before each block, which is why `process` receives it already empty.
    pub fn clear(&mut self) {
        self.events.clear();
    }
}

/// The message format shared by editors and the instance bus.
///
/// It lives here because every adapter and the standalone shell speak it;
/// three private copies of the same parser is three chances for them to
/// drift apart.
pub mod protocol {
    /// Render a parameter change: `"set_param <id> <value>"`.
    pub fn set_param(id: u32, value: f64) -> String {
        format!("set_param {id} {value}")
    }

    /// The JavaScript call editors receive for a parameter value; pages
    /// implement `window.skuizOnParam(id, value)` to receive it.
    pub fn on_param_js(id: u32, value: f64) -> String {
        format!("window.skuizOnParam && window.skuizOnParam({id}, {value})")
    }

    /// Parse a message produced by [`set_param`]. Anything else is `None`.
    pub fn parse_set_param(msg: &str) -> Option<(u32, f64)> {
        let mut it = msg.split_whitespace();
        if it.next() != Some("set_param") {
            return None;
        }
        let parsed = (it.next()?.parse().ok()?, it.next()?.parse().ok()?);
        // Trailing junk means this is not our message.
        if it.next().is_some() {
            return None;
        }
        Some(parsed)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn round_trips_and_rejects_junk() {
            assert_eq!(parse_set_param(&set_param(3, 0.25)), Some((3, 0.25)));
            // A value that needs full precision must survive the trip.
            let v = 0.123456789012345;
            assert_eq!(parse_set_param(&set_param(7, v)), Some((7, v)));
            assert_eq!(parse_set_param("set_param 3"), None);
            assert_eq!(parse_set_param("set_param 3 0.5 extra"), None);
            assert_eq!(parse_set_param("other 3 0.5"), None);
            assert_eq!(parse_set_param("set_param x 0.5"), None);
            assert_eq!(parse_set_param(""), None);
        }
    }
}

/// Snapshot every parameter value under one short lock.
///
/// Adapters use this before pushing values into an editor: the audio thread
/// contends on the same mutex, so the lock must never be held across a
/// webview call of unbounded cost. A poisoned lock is recovered rather than
/// propagated — a panic elsewhere must not cascade into an abort at a
/// plugin's FFI boundary.
pub fn snapshot_params<P: Processor>(processor: &std::sync::Mutex<P>) -> Vec<(u32, f64)> {
    let p = processor.lock().unwrap_or_else(|e| e.into_inner());
    P::params()
        .iter()
        .map(|def| (def.id, p.get_param(def.id)))
        .collect()
}

/// The one trait a Skuiz plugin implements.
///
/// Methods are called from the host's threads per the plugin format's
/// threading rules; `process` is realtime — no allocation or blocking there.
pub trait Processor: Send + 'static {
    /// Static identity and metadata. See [`PluginInfo`].
    ///
    /// Called on any thread, and expected to be a constant.
    fn info() -> PluginInfo
    where
        Self: Sized;

    /// Every parameter this plugin exposes, in the order hosts display them.
    ///
    /// The list is static: parameters cannot be added or removed at runtime,
    /// because hosts snapshot it when the plugin loads. Called on any
    /// thread, including the audio thread, so it must stay cheap.
    fn params() -> &'static [ParamDef]
    where
        Self: Sized;

    /// Prepare for playback at a known sample rate and maximum block size.
    ///
    /// **Main thread.** This is the one place to allocate buffers, build
    /// tables, and reset state — [`Processor::process`] must not. `activate`
    /// may be called again if the host changes sample rate.
    fn activate(&mut self, _sample_rate: f64, _max_frames: u32) {}

    /// Release anything [`Processor::activate`] set up. **Main thread.**
    fn deactivate(&mut self) {}

    /// Apply a parameter change.
    ///
    /// Called from the **audio thread** (host automation, and values
    /// arriving from other instances) *and* the **main thread** (the editor,
    /// state loading), so keep it to arithmetic and assignment: no
    /// allocation, no locking, no I/O. Clamp the value — hosts are not
    /// obliged to respect your declared range.
    fn set_param(&mut self, id: u32, value: f64);

    /// Read a parameter back. Called from **any thread**; same rules as
    /// [`Processor::set_param`].
    fn get_param(&self, id: u32) -> f64;

    /// Process one block of audio, in place.
    ///
    /// `channels[c]` arrives holding the input and must leave holding the
    /// output; every slice is the same length, which is the block size for
    /// this call and varies between calls. `channels` may be empty for a
    /// plugin with no audio output, so a MIDI-only plugin still runs.
    ///
    /// Push any MIDI the DSP generates into `midi`, which arrives cleared;
    /// see [`MidiOut::push`].
    ///
    /// **Audio thread, realtime.** No allocation, no locking, no file or
    /// network I/O, no logging, and no panicking — a panic here crosses an
    /// FFI boundary and aborts the host. Everything expensive belongs in
    /// [`Processor::activate`].
    fn process(&mut self, channels: &mut [&mut [f32]], midi: &mut MidiOut);

    /// Delay this plugin adds between its input and output, in frames.
    ///
    /// Default 0. Adapters report it to the host through the format's
    /// latency mechanism, so the DAW can delay-compensate other tracks.
    /// The value must be constant across the plugin's lifetime: adapters
    /// answer the host's query but never push change notifications, so a
    /// latency that only materialises in [`Processor::activate`] must still
    /// be reported from the start. Called on the **main thread**; keep it
    /// cheap.
    fn latency(&self) -> u32 {
        0
    }

    /// Whether this plugin generates MIDI. Adapters only advertise a note
    /// output port when this is true, so an audio-only plugin doesn't show
    /// hosts a MIDI output that never fires.
    fn emits_midi() -> bool
    where
        Self: Sized,
    {
        false
    }

    /// Static HTML for the plugin editor. `None` (the default) = no GUI.
    fn editor_html() -> Option<&'static str>
    where
        Self: Sized,
    {
        None
    }

    /// Editor size in logical pixels.
    fn editor_size() -> (u32, u32)
    where
        Self: Sized,
    {
        (400, 300)
    }

    /// Serialize state for the DAW project. Default: all param values.
    fn save_state(&self) -> Vec<u8>
    where
        Self: Sized,
    {
        let mut out = Vec::with_capacity(Self::params().len() * 12);
        for p in Self::params() {
            out.extend_from_slice(&p.id.to_le_bytes());
            out.extend_from_slice(&self.get_param(p.id).to_le_bytes());
        }
        out
    }

    /// Restore state saved by [`Processor::save_state`]. Returns false if the
    /// data is not in the expected format; unknown param ids are skipped so
    /// states from other plugin versions still load.
    fn load_state(&mut self, data: &[u8]) -> bool
    where
        Self: Sized,
    {
        if data.is_empty() || !data.len().is_multiple_of(12) {
            return false;
        }
        for chunk in data.chunks_exact(12) {
            let id = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
            let value = f64::from_le_bytes(chunk[4..12].try_into().unwrap());
            if Self::params().iter().any(|p| p.id == id) {
                self.set_param(id, value);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Gain(f64);
    impl Processor for Gain {
        fn info() -> PluginInfo {
            PluginInfo {
                id: "test.gain",
                name: "g",
                vendor: "t",
                version: "0",
                description: "",
            }
        }
        fn params() -> &'static [ParamDef] {
            &[ParamDef {
                id: 7,
                name: "gain",
                min: 0.0,
                max: 1.0,
                default: 0.5,
                choices: &[],
            }]
        }
        fn set_param(&mut self, _id: u32, v: f64) {
            self.0 = v;
        }
        fn get_param(&self, _id: u32) -> f64 {
            self.0
        }
        fn process(&mut self, _channels: &mut [&mut [f32]], _midi: &mut MidiOut) {}
    }

    #[test]
    fn state_roundtrip() {
        let a = Gain(0.25);
        let saved = a.save_state();
        let mut b = Gain(0.9);
        b.load_state(&saved);
        assert_eq!(b.get_param(7), 0.25);
        // corrupt/unknown data must not panic or apply
        let mut c = Gain(0.9);
        assert!(!c.load_state(&[1, 2, 3]));
        assert!(!c.load_state(&[]));
        let mut bad = 99u32.to_le_bytes().to_vec();
        bad.extend_from_slice(&0.1f64.to_le_bytes());
        assert!(c.load_state(&bad)); // valid format, unknown id: ok, skipped
        assert_eq!(c.get_param(7), 0.9);
    }

    #[test]
    fn choice_param_range_and_labels() {
        let cont = ParamDef {
            id: 0,
            name: "g",
            min: -6.0,
            max: 6.0,
            default: 0.0,
            choices: &[],
        };
        assert_eq!((cont.low(), cont.high()), (-6.0, 6.0));
        assert_eq!(cont.label(0.0), None);

        // min/max are ignored for choice params: the range is the index range.
        let modes = ParamDef {
            id: 1,
            name: "Mode",
            min: 99.0,
            max: 99.0,
            default: 0.0,
            choices: &["A", "B", "C"],
        };
        assert_eq!((modes.low(), modes.high()), (0.0, 2.0));
        assert_eq!(modes.label(0.0), Some("A"));
        assert_eq!(modes.label(2.4), Some("C")); // rounds to nearest index
        assert_eq!(modes.label(7.0), None); // out of range, no panic
        assert_eq!(modes.label(-1.0), None);
    }

    #[test]
    fn midi_out_is_bounded_and_never_reallocates() {
        let mut midi = MidiOut::with_capacity(2);
        let ptr = midi.events().as_ptr();
        midi.push(0, MidiEvent::from_midi1([0x90, 60, 100]));
        midi.push(1, MidiEvent::from_midi1([0x80, 60, 0]));
        midi.push(2, MidiEvent::from_midi1([0x90, 62, 100])); // over capacity: dropped, not realloc'd
        assert_eq!(midi.events().len(), 2);
        assert_eq!(midi.events().as_ptr(), ptr, "push must never reallocate");
        assert_eq!(
            midi.events()[0],
            (0, MidiEvent::from_midi1([0x90, 60, 100]))
        );
        midi.clear();
        assert!(midi.events().is_empty());
    }

    #[test]
    fn midi_event_round_trips_midi1_and_ump() {
        let ev = MidiEvent::from_midi1([0x92, 63, 100]);
        assert_eq!(ev.words(), &[0x2092_3F64]);
        assert_eq!(ev.midi1_bytes(), Some([0x92, 63, 100]));

        // A two-word MIDI 2.0 message is not reducible to 3 bytes.
        let wide = MidiEvent::from_ump(&[0x4092_3C00, 0xF800_0000]);
        assert_eq!(wide.words(), &[0x4092_3C00, 0xF800_0000]);
        assert_eq!(wide.midi1_bytes(), None);

        // Over-long input is truncated to four words, never panics.
        assert_eq!(MidiEvent::from_ump(&[0; 6]).words().len(), 4);
    }
}
