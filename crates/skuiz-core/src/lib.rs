//! skuiz-core: the format-agnostic pivot abstraction.
//!
//! Plugin authors implement [`Processor`]; format adapter crates
//! (skuiz-clap, skuiz-vst3, ...) translate host callbacks into calls on it.

#![warn(missing_docs)]

pub mod diag;
pub mod engine;
pub mod lww;
pub mod rt;

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
/// dropdown. Configuration menus — output interface, bit depth, tuning and
/// so on — are all just choice parameters, so they automate, save, and sync
/// over IPC like anything else.
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
    /// Whether this parameter participates in instance sync: editor moves
    /// on a shared parameter broadcast to every other instance (and the
    /// standalone) on the bus; local parameters never leave the instance,
    /// and incoming bus frames for them are ignored. Host automation and
    /// state loads never cross the bus either way (invariant 10).
    pub shared: bool,
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
/// Fixed capacity: [`MidiOut::push`] never allocates and returns `false`
/// once full (invariant 8 — the adapter bumps a diag counter, so the drop
/// is counted, not silent).
pub struct MidiOut {
    events: Vec<(u32, MidiEvent)>,
    /// Events refused because the buffer was full since the last `clear`.
    dropped: usize,
}

impl MidiOut {
    /// Allocate room for `capacity` events. Call off the audio thread.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(capacity),
            dropped: 0,
        }
    }

    /// Queue `event` at `frame` within the current block. Realtime-safe.
    /// Returns `false` when full — the event was not queued.
    pub fn push(&mut self, frame: u32, event: MidiEvent) -> bool {
        if self.events.len() < self.events.capacity() {
            self.events.push((frame, event));
            true
        } else {
            self.dropped += 1;
            false
        }
    }

    /// Events dropped for lack of capacity since the last [`MidiOut::clear`].
    /// Adapters report this through the diag counters (invariant 8).
    pub fn dropped(&self) -> usize {
        self.dropped
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
        self.dropped = 0;
    }
}

/// The message format shared by editors and the instance bus.
///
/// It lives here because every adapter and the standalone shell speak it;
/// three private copies of the same parser is three chances for them to
/// drift apart.
pub mod protocol {
    /// A frame version: `(lamport sequence, origin id)` — see
    /// [`crate::lww`].
    pub type ParamVersion = (u64, u64);

    /// Render a parameter change: `"set_param <id> <value>"`.
    pub fn set_param(id: u32, value: f64) -> String {
        format!("set_param {id} {value}")
    }

    /// Render a versioned parameter change for the bus:
    /// `"set_param <id> <value> <seq> <origin>"`. The version is a lamport
    /// clock plus origin id — see [`crate::lww`]. Editors use the plain
    /// 3-token form (they have no versions); the bus uses this one.
    pub fn set_param_versioned(id: u32, value: f64, seq: u64, origin: u64) -> String {
        format!("set_param {id} {value} {seq} {origin}")
    }

    /// A late joiner's request for current shared state:
    /// `"sync_request <origin>"`. Every instance that hears it answers with
    /// the shared parameters it holds a version for (i.e. ones actually
    /// edited over the bus); last-writer-wins makes duplicate answers safe.
    pub fn sync_request(origin: u64) -> String {
        format!("sync_request {origin}")
    }

    /// A full shared-state answer: `"sync_state <id> <value> <seq>
    /// <origin> ..."` — one quadruple per shared parameter. Frames are
    /// capped at 1 MiB by the transport; a plugin with enough shared
    /// parameters to exceed that has other problems.
    pub fn sync_state(entries: &[(u32, f64, u64, u64)]) -> String {
        let mut out = String::from("sync_state");
        for (id, value, seq, origin) in entries {
            out.push_str(&format!(" {id} {value} {seq} {origin}"));
        }
        out
    }

    /// Parse a bus `set_param` frame, versioned or legacy. The version is
    /// `None` for the plain 3-token form editors and old peers speak.
    pub fn parse_set_param_versioned(msg: &str) -> Option<(u32, f64, Option<ParamVersion>)> {
        let mut it = msg.split_whitespace();
        if it.next() != Some("set_param") {
            return None;
        }
        let id = it.next()?.parse().ok()?;
        let value = it.next()?.parse().ok()?;
        let version = match (it.next(), it.next()) {
            (None, _) => None,
            (Some(seq), Some(origin)) => Some((seq.parse().ok()?, origin.parse().ok()?)),
            (Some(_), None) => return None,
        };
        if it.next().is_some() {
            return None;
        }
        Some((id, value, version))
    }

    /// Parse a [`sync_request`] frame.
    pub fn parse_sync_request(msg: &str) -> Option<u64> {
        let mut it = msg.split_whitespace();
        if it.next() != Some("sync_request") {
            return None;
        }
        let origin = it.next()?.parse().ok()?;
        if it.next().is_some() {
            return None;
        }
        Some(origin)
    }

    /// Parse a [`sync_state`] frame into its `(id, value, seq, origin)`
    /// quadruples.
    pub fn parse_sync_state(msg: &str) -> Option<Vec<(u32, f64, u64, u64)>> {
        let mut it = msg.split_whitespace();
        if it.next() != Some("sync_state") {
            return None;
        }
        let mut out = Vec::new();
        loop {
            match (it.next(), it.next(), it.next(), it.next()) {
                (None, _, _, _) => break,
                (Some(id), Some(value), Some(seq), Some(origin)) => out.push((
                    id.parse().ok()?,
                    value.parse().ok()?,
                    seq.parse().ok()?,
                    origin.parse().ok()?,
                )),
                _ => return None,
            }
        }
        Some(out)
    }

    /// The JavaScript call editors receive for a parameter value; pages
    /// implement `window.skuizOnParam(id, value)` to receive it.
    pub fn on_param_js(id: u32, value: f64) -> String {
        format!("window.skuizOnParam && window.skuizOnParam({id}, {value})")
    }

    /// The page → plugin diagnostics query: the page posts this exact
    /// string and the plugin answers with [`on_diag_js`]. Typed beyond
    /// `set_param` — the editor protocol's second message kind.
    pub const DIAG_QUERY: &str = "skuiz_diag";

    /// The JavaScript call answering a [`DIAG_QUERY`]; pages implement
    /// `window.skuizOnDiag(counters)` to receive it. `counters` is a plain
    /// object mapping counter name to value — see
    /// [`crate::diag::DiagCounters::snapshot`].
    pub fn on_diag_js(diag: &crate::diag::DiagCounters) -> String {
        let mut out = String::from("window.skuizOnDiag && window.skuizOnDiag({");
        for (i, (name, value)) in diag.snapshot().into_iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!("{name}:{value}"));
        }
        out.push_str("})");
        out
    }

    /// Parse a message produced by [`set_param`]. Anything else is `None`.
    pub fn parse_set_param(msg: &str) -> Option<(u32, f64)> {
        let (id, value, version) = parse_set_param_versioned(msg)?;
        // The editor-facing form carries no version tail.
        if version.is_some() {
            return None;
        }
        Some((id, value))
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

        #[test]
        fn versioned_frames_round_trip() {
            let msg = set_param_versioned(3, 0.25, 41, 9);
            assert_eq!(
                parse_set_param_versioned(&msg),
                Some((3, 0.25, Some((41, 9))))
            );
            // The legacy 3-token form parses with no version.
            assert_eq!(
                parse_set_param_versioned("set_param 3 0.25"),
                Some((3, 0.25, None))
            );
            // Editors reject the versioned form, and vice versa.
            assert_eq!(parse_set_param(&msg), None);
            assert_eq!(parse_set_param_versioned("set_param 3 0.25 41"), None);
            assert_eq!(parse_set_param_versioned("set_param 3 0.25 41 9 0"), None);
        }

        #[test]
        fn sync_frames_round_trip() {
            assert_eq!(parse_sync_request(&sync_request(77)), Some(77));
            assert_eq!(parse_sync_request("sync_request"), None);

            let entries = [(1, 0.5, 3, 10), (2, 1.0, 4, 11)];
            assert_eq!(
                parse_sync_state(&sync_state(&entries)),
                Some(entries.to_vec())
            );
            assert_eq!(parse_sync_state("sync_state 1 0.5 3"), None);
            assert_eq!(parse_sync_state(&sync_state(&[])), Some(vec![]));
        }

        #[test]
        fn diag_query_answers_with_a_guarded_js_object() {
            let diag = crate::diag::DiagCounters::default();
            crate::diag::DiagCounters::bump(&diag.midi_events_dropped);
            let js = on_diag_js(&diag);
            assert!(js.starts_with("window.skuizOnDiag && window.skuizOnDiag({"));
            assert!(js.contains("midi_events_dropped:1"));
            assert!(js.contains("commands_dropped:0"));
            assert!(js.ends_with("})"));
            // The query itself must not parse as a parameter change.
            assert_eq!(parse_set_param(DIAG_QUERY), None);
            assert_eq!(parse_set_param_versioned(DIAG_QUERY), None);
        }
    }
}

/// Whether `id` names a shared parameter of `P`: one whose editor moves
/// sync across instances over the bus (see [`ParamDef::shared`]). Adapters
/// filter both broadcast and receive through this, so local parameters
/// never leave the instance and bus frames for them are ignored.
pub fn syncs_over_bus<P: Processor>(id: u32) -> bool {
    P::params().iter().any(|p| p.id == id && p.shared)
}

/// The one trait a Skuiz plugin implements.
///
/// The engine ([`crate::engine`]) owns every instance and enforces the
/// threading contract: while blocks flow, the **audio thread** has exclusive
/// access to the processor; while stopped, the **main thread** does. Hosts,
/// editors and the IPC bus never call into the processor directly while
/// running — parameter changes travel the engine's realtime-safe command
/// queue, and reads are answered by its parameter mirror. See
/// `docs/concepts/invariants.md` for the rules this rests on.
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

    /// Reset DSP state — delay lines, envelopes, filter memory, LFO phases —
    /// without touching parameter values. Hosts call this when the transport
    /// jumps or a unit is recycled, so after `reset` the plugin must sound
    /// as if freshly [`Processor::activate`]d with the current parameters.
    ///
    /// Called on the **audio thread** between blocks while running, on the
    /// **main thread** when stopped; same realtime rules as
    /// [`Processor::set_param`]. Default: no state, nothing to do.
    fn reset(&mut self) {}

    /// Apply a parameter change.
    ///
    /// Called on the **audio thread** while blocks flow (host automation,
    /// plus editor and bus changes replayed from the engine's command
    /// queue) and on the **main thread** when the transport is stopped.
    /// Either way, keep it to arithmetic and assignment: no allocation, no
    /// locking, no I/O. Clamp the value — hosts are not obliged to respect
    /// your declared range.
    fn set_param(&mut self, id: u32, value: f64);

    /// Read a parameter back. Same threading as [`Processor::set_param`].
    /// While blocks flow, hosts and editors do **not** call this — they read
    /// the engine's parameter mirror instead.
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
    ///
    /// It **may change at runtime**: the engine re-reads it once per block
    /// and, on change, updates the reported value and notifies the host
    /// (CLAP `clap_host_latency.changed`, VST3 `kLatencyChanged`; AUv3 and
    /// the standalone shell report no change notification). Because of that
    /// poll it is also called on the **audio thread** — keep it
    /// realtime-safe: no allocation, no locking, just read a field.
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

    /// Serialize state for the DAW project. Default: a versioned header
    /// followed by all param values.
    ///
    /// The default format is `b"SKZ1"` followed by `(id: u32 LE, value:
    /// f64 LE)` pairs. The magic's last byte is the format version: if the
    /// default format ever changes, the version bumps and older versions
    /// stay loadable. **If you override `save_state`, version your own
    /// format the same way** — hosts keep project files for years.
    ///
    /// Called on the **main thread** when stopped; while running, the engine
    /// routes it onto the audio thread between blocks. Allocation is
    /// explicitly allowed here — the one exception to the audio-thread
    /// rules (invariant 3).
    fn save_state(&self) -> Vec<u8>
    where
        Self: Sized,
    {
        let mut out = Vec::with_capacity(STATE_MAGIC.len() + Self::params().len() * 12);
        out.extend_from_slice(STATE_MAGIC);
        for p in Self::params() {
            out.extend_from_slice(&p.id.to_le_bytes());
            out.extend_from_slice(&self.get_param(p.id).to_le_bytes());
        }
        out
    }

    /// Restore state saved by [`Processor::save_state`]. Returns false if the
    /// data is not in the expected format; unknown param ids are skipped so
    /// states from other plugin versions still load.
    ///
    /// The default loader accepts both the versioned format and the legacy
    /// pre-versioning raw `(id, value)` pairs, so projects saved by older
    /// builds keep loading. (One collision: a legacy buffer is mistaken for
    /// a versioned one if the first saved param id is `0x315A4B53` —
    /// `"SKZ1"` little-endian. Don't use that id.)
    ///
    /// Same threading as [`Processor::save_state`]; the same allocation
    /// exception applies.
    fn load_state(&mut self, data: &[u8]) -> bool
    where
        Self: Sized,
    {
        let data = data.strip_prefix(STATE_MAGIC).unwrap_or(data);
        if data.is_empty() || !data.len().is_multiple_of(12) {
            return false;
        }
        for chunk in data.as_chunks::<12>().0 {
            let id = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
            let value = f64::from_le_bytes(chunk[4..12].try_into().unwrap());
            if Self::params().iter().any(|p| p.id == id) {
                self.set_param(id, value);
            }
        }
        true
    }
}

/// Magic header of the default state format; the last byte is the format
/// version. See [`Processor::save_state`].
pub const STATE_MAGIC: &[u8; 4] = b"SKZ1";

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
                shared: true,
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
        assert!(saved.starts_with(STATE_MAGIC), "state must be versioned");
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
    fn legacy_unversioned_state_still_loads() {
        // The pre-versioning format: bare (id, value) pairs, no header.
        let mut legacy = 7u32.to_le_bytes().to_vec();
        legacy.extend_from_slice(&0.25f64.to_le_bytes());
        let mut p = Gain(0.9);
        assert!(p.load_state(&legacy));
        assert_eq!(p.get_param(7), 0.25);
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
            shared: true,
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
            shared: true,
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
