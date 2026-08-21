//! Declarative audio-bus topology: static per-plugin declarations translated
//! by each adapter into the host's native bus model.
//!
//! A plugin declares its buses once via [`Processor::audio_buses`] —
//! `&'static` metadata, like [`crate::ParamDef`]. The topology is immutable
//! at runtime: hosts may activate or deactivate an *optional* bus, but no
//! bus is ever created or destroyed after load. All negotiation happens in
//! the adapters on the main thread; the audio path only fills preallocated
//! pointer scratch ([`TopologyScratch`]) and builds stack views
//! ([`AudioInputs`]/[`AudioOutputs`]) over it — no allocation, locking, or
//! string lookup while rendering.
//!
//! [`Processor::audio_buses`]: crate::Processor::audio_buses

/// Most channels a single bus may carry. Surround layouts land later; this
/// bounds the per-block stack views so they never allocate.
pub const MAX_BUS_CHANNELS: usize = 8;

/// Most buses per direction. Generous for any realistic topology (main plus
/// a sidechain or two); enforced by [`validate_buses`].
pub const MAX_BUSES_PER_DIRECTION: usize = 4;

/// Stable identifier for a declared bus.
///
/// Like [`crate::ParamDef::id`], a bus id must never change once the plugin
/// ships: hosts key bus state (activation, saved routings) on it. The
/// default id is a const FNV-1a hash of the bus name, so declarative names
/// resolve to static ids at compile time; override with
/// [`AudioBusSpec::with_id`] to keep an id stable across a rename.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BusId(pub u32);

impl BusId {
    /// FNV-1a over the name bytes — the same hash family the VST3 adapter
    /// uses for class ids, but `const`, so it runs at compile time.
    pub const fn from_name(name: &str) -> Self {
        let bytes = name.as_bytes();
        let mut hash = 0x811c_9dc5u32;
        let mut i = 0;
        while i < bytes.len() {
            hash = (hash ^ bytes[i] as u32).wrapping_mul(0x0100_0193);
            i += 1;
        }
        BusId(hash)
    }
}

/// Which way a bus flows, from the plugin's point of view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BusDirection {
    /// Audio the host feeds the plugin.
    Input,
    /// Audio the plugin produces.
    Output,
}

/// Channel arrangement of a bus.
///
/// Named surround layouts are deliberately deferred; `Discrete` covers any
/// channel count until then.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelLayout {
    /// One channel.
    Mono,
    /// Two channels, left/right.
    Stereo,
    /// `n` unlabeled channels.
    Discrete(u8),
}

impl ChannelLayout {
    /// Channel count of this layout.
    pub const fn channels(self) -> u8 {
        match self {
            ChannelLayout::Mono => 1,
            ChannelLayout::Stereo => 2,
            ChannelLayout::Discrete(n) => n,
        }
    }
}

/// One statically declared audio bus.
///
/// Build these in const context with [`AudioBusSpec::input`] /
/// [`AudioBusSpec::output`]; the declaration is the single source of truth
/// every adapter translates from.
pub struct AudioBusSpec {
    /// Stable id — see [`BusId`].
    pub id: BusId,
    /// Display name hosts show ("Main", "Sidechain").
    pub name: &'static str,
    /// Input or output.
    pub direction: BusDirection,
    /// Channel arrangement.
    pub layout: ChannelLayout,
    /// Whether the host may leave this bus inactive. Optional means "this
    /// statically defined bus may currently be inactive", never "the
    /// topology changes". Only non-main buses may be optional.
    pub optional: bool,
}

impl AudioBusSpec {
    /// A declared input bus, active by default, id hashed from `name`.
    pub const fn input(name: &'static str, layout: ChannelLayout) -> Self {
        Self {
            id: BusId::from_name(name),
            name,
            direction: BusDirection::Input,
            layout,
            optional: false,
        }
    }

    /// A declared output bus, active by default, id hashed from `name`.
    pub const fn output(name: &'static str, layout: ChannelLayout) -> Self {
        Self {
            id: BusId::from_name(name),
            name,
            direction: BusDirection::Output,
            layout,
            optional: false,
        }
    }

    /// Mark this bus optional: the host may deactivate it (or never connect
    /// it); the processor sees it as [`InputBus::active`] `false`.
    pub const fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    /// Pin an explicit id, e.g. to keep saved projects valid across a
    /// rename of the bus.
    pub const fn with_id(mut self, id: u32) -> Self {
        self.id = BusId(id);
        self
    }
}

/// Why a topology declaration is invalid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BusError {
    /// Two buses in the same direction share an id.
    DuplicateId,
    /// The first bus of a direction — the main bus — is marked optional.
    MainIsOptional,
    /// More than [`MAX_BUSES_PER_DIRECTION`] buses in one direction.
    TooManyBuses,
    /// `Discrete(0)` or a layout over [`MAX_BUS_CHANNELS`].
    BadChannelCount,
}

/// Check a declaration. `Engine::new` debug-asserts this and treats an
/// invalid topology as empty in release builds (fail safe, deterministic);
/// adapters only ever see validated scratch.
pub fn validate_buses(specs: &[AudioBusSpec]) -> Result<(), BusError> {
    for dir in [BusDirection::Input, BusDirection::Output] {
        let mut count = 0usize;
        for (i, spec) in specs.iter().filter(|s| s.direction == dir).enumerate() {
            count += 1;
            if i == 0 && spec.optional {
                return Err(BusError::MainIsOptional);
            }
            if specs
                .iter()
                .filter(|s| s.direction == dir)
                .skip(i + 1)
                .any(|s| s.id == spec.id)
            {
                return Err(BusError::DuplicateId);
            }
            let n = spec.layout.channels();
            if n == 0 || n as usize > MAX_BUS_CHANNELS {
                return Err(BusError::BadChannelCount);
            }
        }
        if count > MAX_BUSES_PER_DIRECTION {
            return Err(BusError::TooManyBuses);
        }
    }
    Ok(())
}

/// Default topology: one stereo main input, one stereo main output — an
/// effect. This is exactly what every adapter hardcoded before topologies
/// were declarative, so existing processors keep their host-facing behavior
/// by declaring nothing.
pub static DEFAULT_EFFECT_BUSES: &[AudioBusSpec] = &[
    AudioBusSpec::input("Main", ChannelLayout::Stereo),
    AudioBusSpec::output("Main", ChannelLayout::Stereo),
];

/// Instrument topology: no inputs, one stereo main output.
pub static INSTRUMENT_BUSES: &[AudioBusSpec] =
    &[AudioBusSpec::output("Main", ChannelLayout::Stereo)];

/// Read-only view of one input bus for the current block.
///
/// Channels borrow the host's buffers directly; an inactive bus reports no
/// channels at all.
pub struct InputBus<'a> {
    spec: &'static AudioBusSpec,
    active: bool,
    channels: [&'a [f32]; MAX_BUS_CHANNELS],
    n_channels: usize,
}

impl<'a> InputBus<'a> {
    fn empty() -> Self {
        Self {
            spec: &EMPTY_SPEC,
            active: false,
            channels: [&[]; MAX_BUS_CHANNELS],
            n_channels: 0,
        }
    }

    /// The static declaration this view comes from.
    pub fn spec(&self) -> &'static AudioBusSpec {
        self.spec
    }

    /// Stable id of this bus.
    pub fn id(&self) -> BusId {
        self.spec.id
    }

    /// Whether the host has this bus connected/active this block. An
    /// inactive bus yields no channels.
    pub fn active(&self) -> bool {
        self.active
    }

    /// Declared channel layout.
    pub fn layout(&self) -> ChannelLayout {
        self.spec.layout
    }

    /// The channels actually present this block (empty when inactive, at
    /// most `layout().channels()` long).
    pub fn channels(&self) -> &[&'a [f32]] {
        &self.channels[..self.n_channels]
    }

    /// Channel `i`, if present.
    pub fn channel(&self, i: usize) -> Option<&'a [f32]> {
        self.channels().get(i).copied()
    }
}

/// Read-write view of one output bus for the current block.
pub struct OutputBus<'a> {
    spec: &'static AudioBusSpec,
    active: bool,
    channels: [&'a mut [f32]; MAX_BUS_CHANNELS],
    n_channels: usize,
}

impl<'a> OutputBus<'a> {
    fn empty() -> Self {
        Self {
            spec: &EMPTY_SPEC,
            active: false,
            channels: std::array::from_fn(|_| &mut [] as &mut [f32]),
            n_channels: 0,
        }
    }

    /// The static declaration this view comes from.
    pub fn spec(&self) -> &'static AudioBusSpec {
        self.spec
    }

    /// Stable id of this bus.
    pub fn id(&self) -> BusId {
        self.spec.id
    }

    /// Whether the host has this bus connected/active this block.
    pub fn active(&self) -> bool {
        self.active
    }

    /// Declared channel layout.
    pub fn layout(&self) -> ChannelLayout {
        self.spec.layout
    }

    /// The channels actually present this block.
    pub fn channels(&mut self) -> &mut [&'a mut [f32]] {
        &mut self.channels[..self.n_channels]
    }

    /// Channel `i`, if present.
    pub fn channel_mut(&mut self, i: usize) -> Option<&mut [f32]> {
        self.channels.get_mut(i).map(|c| &mut **c)
    }

    /// Frames in this block (0 when the bus is inactive).
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, |c| c.len())
    }
}

/// All input buses for one block, in declaration order. Built on the stack
/// per block — no allocation.
pub struct AudioInputs<'a> {
    buses: [InputBus<'a>; MAX_BUSES_PER_DIRECTION],
    len: usize,
}

impl<'a> AudioInputs<'a> {
    fn empty() -> Self {
        Self {
            buses: std::array::from_fn(|_| InputBus::empty()),
            len: 0,
        }
    }

    /// Number of declared input buses (active or not).
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no input buses are declared (instruments).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The main input bus — the first declared — if any.
    pub fn main(&self) -> Option<&InputBus<'a>> {
        self.buses[..self.len].first()
    }

    /// Bus by stable id.
    pub fn get(&self, id: BusId) -> Option<&InputBus<'a>> {
        self.buses[..self.len].iter().find(|b| b.id() == id)
    }

    /// Bus by declaration index.
    pub fn at(&self, index: usize) -> Option<&InputBus<'a>> {
        self.buses[..self.len].get(index)
    }

    /// Iterate declared buses in order.
    pub fn iter(&self) -> impl Iterator<Item = &InputBus<'a>> {
        self.buses[..self.len].iter()
    }
}

/// All output buses for one block, in declaration order.
pub struct AudioOutputs<'a> {
    buses: [OutputBus<'a>; MAX_BUSES_PER_DIRECTION],
    len: usize,
}

impl<'a> AudioOutputs<'a> {
    fn empty() -> Self {
        Self {
            buses: std::array::from_fn(|_| OutputBus::empty()),
            len: 0,
        }
    }

    /// Number of declared output buses.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no output buses are declared.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The main output bus — the first declared — if any.
    pub fn main(&mut self) -> Option<&mut OutputBus<'a>> {
        self.buses[..self.len].first_mut()
    }

    /// Bus by stable id.
    pub fn get(&mut self, id: BusId) -> Option<&mut OutputBus<'a>> {
        self.buses[..self.len].iter_mut().find(|b| b.id() == id)
    }

    /// Bus by declaration index.
    pub fn at(&mut self, index: usize) -> Option<&mut OutputBus<'a>> {
        self.buses[..self.len].get_mut(index)
    }

    /// Iterate declared buses in order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut OutputBus<'a>> {
        self.buses[..self.len].iter_mut()
    }
}

/// Placeholder spec for unused view slots; never observable.
static EMPTY_SPEC: AudioBusSpec = AudioBusSpec::input("", ChannelLayout::Mono);

/// One channel's host buffer, raw so the scratch can outlive any single
/// block borrow. Filled by the adapter at the top of every block/segment.
struct ChannelSlot {
    ptr: *mut f32,
    len: usize,
}

impl ChannelSlot {
    const fn empty() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            len: 0,
        }
    }
}

struct BusScratch {
    active: bool,
    channels: [ChannelSlot; MAX_BUS_CHANNELS],
    n_channels: usize,
}

impl BusScratch {
    fn new() -> Self {
        Self {
            active: false,
            channels: std::array::from_fn(|_| ChannelSlot::empty()),
            n_channels: 0,
        }
    }
}

/// Audio-thread bus scaffolding owned by the engine
/// ([`crate::engine::AudioCore`]): one slot per declared bus, allocated on
/// the main thread at engine construction. Adapters rewrite pointers at the
/// top of each block and build views over it — the audio path never
/// allocates.
///
/// All methods are audio-thread-only under the `AudioToken` protocol, same
/// as the rest of `AudioCore`.
pub struct TopologyScratch {
    specs: &'static [AudioBusSpec],
    inputs: Vec<BusScratch>,
    outputs: Vec<BusScratch>,
}

// SAFETY: accessed only from the audio thread, reached through the engine's
// token-gated `AudioCore`; raw pointers name host buffers valid for the
// current block.
unsafe impl Send for TopologyScratch {}

impl TopologyScratch {
    /// Size the scratch from a declaration. Main thread only (allocates).
    /// An invalid declaration yields an empty scratch — fail safe.
    /// (`Engine::new` debug-asserts validity at the authoring site.)
    pub fn new(specs: &'static [AudioBusSpec]) -> Self {
        let count = |dir: BusDirection| {
            if validate_buses(specs).is_ok() {
                specs.iter().filter(|s| s.direction == dir).count()
            } else {
                0
            }
        };
        Self {
            specs: if validate_buses(specs).is_ok() {
                specs
            } else {
                &[]
            },
            inputs: (0..count(BusDirection::Input))
                .map(|_| BusScratch::new())
                .collect(),
            outputs: (0..count(BusDirection::Output))
                .map(|_| BusScratch::new())
                .collect(),
        }
    }

    /// The validated declaration (empty if the declared topology was
    /// invalid).
    pub fn specs(&self) -> &'static [AudioBusSpec] {
        self.specs
    }

    fn slots_mut(&mut self, dir: BusDirection) -> &mut [BusScratch] {
        match dir {
            BusDirection::Input => &mut self.inputs,
            BusDirection::Output => &mut self.outputs,
        }
    }

    /// Number of declared buses in a direction.
    pub fn bus_count(&self, dir: BusDirection) -> usize {
        match dir {
            BusDirection::Input => self.inputs.len(),
            BusDirection::Output => self.outputs.len(),
        }
    }

    /// Mark every bus inactive with no channels. Adapters call this at the
    /// top of each block, then activate what the host connected.
    pub fn clear(&mut self) {
        for slot in self.inputs.iter_mut().chain(self.outputs.iter_mut()) {
            slot.active = false;
            slot.n_channels = 0;
        }
    }

    /// Mark a bus active/inactive for this block.
    pub fn set_active(&mut self, dir: BusDirection, bus: usize, active: bool) {
        if let Some(slot) = self.slots_mut(dir).get_mut(bus) {
            slot.active = active;
        }
    }

    /// Point one channel of a bus at a host buffer for this block.
    ///
    /// # Safety
    /// `ptr..ptr+len` must be readable (inputs) or writable (outputs) for
    /// the rest of the current process call, and exclusively writable for
    /// outputs.
    pub unsafe fn set_channel(
        &mut self,
        dir: BusDirection,
        bus: usize,
        channel: usize,
        ptr: *mut f32,
        len: usize,
    ) {
        let Some(slot) = self.slots_mut(dir).get_mut(bus) else {
            return;
        };
        if channel >= MAX_BUS_CHANNELS || ptr.is_null() {
            return;
        }
        slot.channels[channel] = ChannelSlot { ptr, len };
        slot.n_channels = slot.n_channels.max(channel + 1);
    }

    /// Build this block's views over the scratch. Stack-built, no
    /// allocation; the mutable borrow ties the views' lifetimes to the
    /// scratch so the buffers cannot be repointed while a view is alive.
    pub fn views(&mut self) -> (AudioInputs<'_>, AudioOutputs<'_>) {
        // `specs` is `&'static`, so copying it out frees the borrow of
        // `self` for the scratch loops below.
        let specs = self.specs;
        let spec_at =
            |dir: BusDirection, i: usize| specs.iter().filter(move |s| s.direction == dir).nth(i);

        let mut inputs = AudioInputs::empty();
        for (i, slot) in self.inputs.iter_mut().enumerate() {
            let Some(spec) = spec_at(BusDirection::Input, i) else {
                break;
            };
            let mut view = InputBus {
                spec,
                active: slot.active,
                channels: [&[]; MAX_BUS_CHANNELS],
                n_channels: 0,
            };
            if slot.active {
                for (c, ch) in slot.channels.iter().enumerate().take(slot.n_channels) {
                    if !ch.ptr.is_null() {
                        // SAFETY: per set_channel's contract the adapter
                        // guarantees this buffer for the current block.
                        view.channels[c] = unsafe { std::slice::from_raw_parts(ch.ptr, ch.len) };
                        view.n_channels = c + 1;
                    }
                }
            }
            inputs.buses[i] = view;
            inputs.len = i + 1;
        }

        let mut outputs = AudioOutputs::empty();
        for (i, slot) in self.outputs.iter_mut().enumerate() {
            let Some(spec) = spec_at(BusDirection::Output, i) else {
                break;
            };
            let mut view = OutputBus {
                spec,
                active: slot.active,
                channels: std::array::from_fn(|_| &mut [] as &mut [f32]),
                n_channels: 0,
            };
            if slot.active {
                for (c, ch) in slot.channels.iter_mut().enumerate().take(slot.n_channels) {
                    if !ch.ptr.is_null() {
                        // SAFETY: per set_channel's contract the adapter
                        // guarantees exclusive access for the current block.
                        view.channels[c] =
                            unsafe { std::slice::from_raw_parts_mut(ch.ptr, ch.len) };
                        view.n_channels = c + 1;
                    }
                }
            }
            outputs.buses[i] = view;
            outputs.len = i + 1;
        }

        (inputs, outputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIDECHAIN_FX: &[AudioBusSpec] = &[
        AudioBusSpec::input("Main", ChannelLayout::Stereo),
        AudioBusSpec::input("Sidechain", ChannelLayout::Mono).optional(),
        AudioBusSpec::output("Main", ChannelLayout::Stereo),
    ];

    #[test]
    fn ids_are_stable_and_name_derived() {
        assert_eq!(BusId::from_name("Main"), BusId::from_name("Main"));
        assert_ne!(BusId::from_name("Main"), BusId::from_name("Sidechain"));
        // with_id pins an explicit value.
        let spec = AudioBusSpec::input("Main", ChannelLayout::Stereo).with_id(42);
        assert_eq!(spec.id, BusId(42));
    }

    #[test]
    fn layouts_report_channel_counts() {
        assert_eq!(ChannelLayout::Mono.channels(), 1);
        assert_eq!(ChannelLayout::Stereo.channels(), 2);
        assert_eq!(ChannelLayout::Discrete(6).channels(), 6);
    }

    #[test]
    fn defaults_are_valid_and_match_legacy_behavior() {
        assert!(validate_buses(DEFAULT_EFFECT_BUSES).is_ok());
        assert!(validate_buses(INSTRUMENT_BUSES).is_ok());
        assert_eq!(
            DEFAULT_EFFECT_BUSES
                .iter()
                .filter(|s| s.direction == BusDirection::Input)
                .count(),
            1
        );
        assert_eq!(
            INSTRUMENT_BUSES
                .iter()
                .filter(|s| s.direction == BusDirection::Input)
                .count(),
            0
        );
    }

    #[test]
    fn validation_rejects_bad_topologies() {
        // Duplicate ids within a direction.
        const DUP: &[AudioBusSpec] = &[
            AudioBusSpec::input("Main", ChannelLayout::Stereo),
            AudioBusSpec::input("Main", ChannelLayout::Mono),
        ];
        assert_eq!(validate_buses(DUP), Err(BusError::DuplicateId));
        // Same id across directions is fine.
        const MAIN_MAIN: &[AudioBusSpec] = &[
            AudioBusSpec::input("Main", ChannelLayout::Stereo),
            AudioBusSpec::output("Main", ChannelLayout::Stereo),
        ];
        assert!(validate_buses(MAIN_MAIN).is_ok());
        // Main bus cannot be optional.
        const OPT_MAIN: &[AudioBusSpec] =
            &[AudioBusSpec::input("Main", ChannelLayout::Stereo).optional()];
        assert_eq!(validate_buses(OPT_MAIN), Err(BusError::MainIsOptional));
        // Channel counts are bounded.
        const ZERO: &[AudioBusSpec] = &[AudioBusSpec::output("Main", ChannelLayout::Discrete(0))];
        assert_eq!(validate_buses(ZERO), Err(BusError::BadChannelCount));
        const HUGE: &[AudioBusSpec] = &[AudioBusSpec::output("Main", ChannelLayout::Discrete(9))];
        assert_eq!(validate_buses(HUGE), Err(BusError::BadChannelCount));
        // Too many buses in one direction.
        const MANY: &[AudioBusSpec] = &[
            AudioBusSpec::input("A", ChannelLayout::Mono),
            AudioBusSpec::input("B", ChannelLayout::Mono),
            AudioBusSpec::input("C", ChannelLayout::Mono),
            AudioBusSpec::input("D", ChannelLayout::Mono),
            AudioBusSpec::input("E", ChannelLayout::Mono),
        ];
        assert_eq!(validate_buses(MANY), Err(BusError::TooManyBuses));
        assert!(validate_buses(SIDECHAIN_FX).is_ok());
    }

    #[test]
    fn views_reflect_what_the_adapter_filled() {
        let mut left_in = [1.0f32; 8];
        let mut right_in = [2.0f32; 8];
        let mut side = [0.5f32; 8];
        let mut left_out = [0.0f32; 8];
        let mut right_out = [0.0f32; 8];

        let mut scratch = TopologyScratch::new(SIDECHAIN_FX);
        assert_eq!(scratch.bus_count(BusDirection::Input), 2);
        assert_eq!(scratch.bus_count(BusDirection::Output), 1);

        scratch.clear();
        scratch.set_active(BusDirection::Input, 0, true);
        scratch.set_active(BusDirection::Input, 1, true);
        scratch.set_active(BusDirection::Output, 0, true);
        unsafe {
            scratch.set_channel(BusDirection::Input, 0, 0, left_in.as_mut_ptr(), 8);
            scratch.set_channel(BusDirection::Input, 0, 1, right_in.as_mut_ptr(), 8);
            scratch.set_channel(BusDirection::Input, 1, 0, side.as_mut_ptr(), 8);
            scratch.set_channel(BusDirection::Output, 0, 0, left_out.as_mut_ptr(), 8);
            scratch.set_channel(BusDirection::Output, 0, 1, right_out.as_mut_ptr(), 8);
        }

        let (inputs, mut outputs) = scratch.views();
        let main = inputs.main().expect("main input");
        assert!(main.active());
        assert_eq!(main.layout(), ChannelLayout::Stereo);
        assert_eq!(main.channel(0).unwrap(), &[1.0f32; 8]);
        let side = inputs
            .get(BusId::from_name("Sidechain"))
            .expect("sidechain");
        assert!(side.active());
        assert_eq!(side.channel(0).unwrap()[0], 0.5);

        let out = outputs.main().expect("main output");
        assert_eq!(out.frames(), 8);
        out.channel_mut(0).unwrap()[0] = 42.0;
        assert_eq!(left_out[0], 42.0);
    }

    #[test]
    fn inactive_optional_bus_yields_no_channels() {
        let mut left_in = [1.0f32; 4];
        let mut left_out = [0.0f32; 4];
        let mut scratch = TopologyScratch::new(SIDECHAIN_FX);
        scratch.clear();
        scratch.set_active(BusDirection::Input, 0, true);
        scratch.set_active(BusDirection::Output, 0, true);
        unsafe {
            scratch.set_channel(BusDirection::Input, 0, 0, left_in.as_mut_ptr(), 4);
            scratch.set_channel(BusDirection::Output, 0, 0, left_out.as_mut_ptr(), 4);
        }
        let (inputs, _) = scratch.views();
        let side = inputs
            .get(BusId::from_name("Sidechain"))
            .expect("sidechain");
        assert!(!side.active());
        assert!(side.channels().is_empty());
        assert_eq!(side.channel(0), None);
    }

    #[test]
    fn instrument_topology_has_no_inputs() {
        let mut left_out = [0.0f32; 4];
        let mut scratch = TopologyScratch::new(INSTRUMENT_BUSES);
        scratch.clear();
        scratch.set_active(BusDirection::Output, 0, true);
        unsafe {
            scratch.set_channel(BusDirection::Output, 0, 0, left_out.as_mut_ptr(), 4);
        }
        let (inputs, outputs) = scratch.views();
        assert!(inputs.is_empty());
        assert!(inputs.main().is_none());
        assert_eq!(outputs.len(), 1);
    }

    #[test]
    fn invalid_topology_fails_safe_to_empty() {
        const BAD: &[AudioBusSpec] = &[
            AudioBusSpec::input("Main", ChannelLayout::Stereo),
            AudioBusSpec::input("Main", ChannelLayout::Mono),
        ];
        let mut scratch = TopologyScratch::new(BAD);
        assert_eq!(scratch.bus_count(BusDirection::Input), 0);
        let (inputs, outputs) = scratch.views();
        assert!(inputs.is_empty());
        assert!(outputs.is_empty());
    }
}
