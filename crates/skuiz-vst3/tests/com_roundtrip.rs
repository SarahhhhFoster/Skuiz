//! Drives the VST3 adapter the way a host does: through the factory, the
//! COM interfaces, real audio buffers, and a state stream.

#![allow(non_snake_case)]

use skuiz_core::{MidiOut, ParamDef, PluginInfo, Processor};
use skuiz_vst3::vst3::Steinberg::Vst::*;
use skuiz_vst3::vst3::Steinberg::*;
use skuiz_vst3::vst3::{Class, ComPtr, ComWrapper, Interface};
use skuiz_vst3::{derive_cid, Vst3Factory, Vst3Plugin};
use std::cell::RefCell;
use std::ffi::c_void;

/// A gain that also emits one note per block, so the same test covers audio,
/// parameters, and MIDI conversion.
struct Fixture {
    gain: f64,
    mode: f64,
}

impl Default for Fixture {
    fn default() -> Self {
        Self {
            gain: 1.0,
            mode: 0.0,
        }
    }
}

impl Processor for Fixture {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "test.vst3fixture",
            name: "Fixture",
            vendor: "Skuiz",
            version: "0",
            description: "",
        }
    }
    fn params() -> &'static [ParamDef] {
        &[
            ParamDef {
                id: 0,
                name: "Gain",
                min: 0.0,
                max: 2.0,
                default: 1.0,
                choices: &[],
            },
            ParamDef {
                id: 1,
                name: "Mode",
                min: 0.0,
                max: 0.0,
                default: 0.0,
                choices: &["Off", "On", "Auto"],
            },
        ]
    }
    fn emits_midi() -> bool {
        true
    }
    fn editor_html() -> Option<&'static str> {
        Some("<!doctype html><body>fixture</body>")
    }
    fn editor_size() -> (u32, u32) {
        (321, 123)
    }
    fn set_param(&mut self, id: u32, v: f64) {
        match id {
            0 => self.gain = v,
            1 => self.mode = v,
            _ => {}
        }
    }
    fn get_param(&self, id: u32) -> f64 {
        match id {
            0 => self.gain,
            1 => self.mode,
            _ => 0.0,
        }
    }
    fn process(&mut self, channels: &mut [&mut [f32]], midi: &mut MidiOut) {
        let g = self.gain as f32;
        for ch in channels.iter_mut() {
            for s in ch.iter_mut() {
                *s *= g;
            }
        }
        midi.push(0, [0x90, 64, 100]);
    }
}

/// Minimal in-memory IBStream, so state can be saved and reloaded without a host.
struct MemStream {
    data: RefCell<Vec<u8>>,
    pos: RefCell<usize>,
}

impl MemStream {
    fn new(data: Vec<u8>) -> Self {
        Self {
            data: RefCell::new(data),
            pos: RefCell::new(0),
        }
    }
}

impl Class for MemStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemStream {
    unsafe fn read(
        &self,
        buffer: *mut c_void,
        numBytes: int32,
        numBytesRead: *mut int32,
    ) -> tresult {
        let data = self.data.borrow();
        let mut pos = self.pos.borrow_mut();
        let n = (numBytes.max(0) as usize).min(data.len() - *pos);
        std::ptr::copy_nonoverlapping(data[*pos..].as_ptr(), buffer as *mut u8, n);
        *pos += n;
        if !numBytesRead.is_null() {
            *numBytesRead = n as int32;
        }
        kResultOk
    }

    unsafe fn write(
        &self,
        buffer: *mut c_void,
        numBytes: int32,
        numBytesWritten: *mut int32,
    ) -> tresult {
        let n = numBytes.max(0) as usize;
        let src = std::slice::from_raw_parts(buffer as *const u8, n);
        self.data.borrow_mut().extend_from_slice(src);
        if !numBytesWritten.is_null() {
            *numBytesWritten = n as int32;
        }
        kResultOk
    }

    unsafe fn seek(&self, _pos: int64, _mode: int32, _result: *mut int64) -> tresult {
        kNotImplemented
    }

    unsafe fn tell(&self, _pos: *mut int64) -> tresult {
        kNotImplemented
    }
}

/// Collects events the plugin emits.
#[derive(Default)]
struct EventSink {
    events: RefCell<Vec<Event>>,
}

impl Class for EventSink {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for EventSink {
    unsafe fn getEventCount(&self) -> int32 {
        self.events.borrow().len() as int32
    }
    unsafe fn getEvent(&self, index: int32, e: *mut Event) -> tresult {
        match self.events.borrow().get(index as usize) {
            Some(found) if !e.is_null() => {
                *e = *found;
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }
    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        if e.is_null() {
            return kInvalidArgument;
        }
        self.events.borrow_mut().push(*e);
        kResultOk
    }
}

/// Create a plugin instance through the factory, exactly as a host would.
unsafe fn instantiate() -> ComPtr<IComponent> {
    let factory = ComWrapper::new(Vst3Factory::<Fixture>::default())
        .to_com_ptr::<IPluginFactory>()
        .unwrap();

    let count = factory.countClasses();
    assert_eq!(count, 1, "single-component plugin exposes one class");

    let mut info: PClassInfo = std::mem::zeroed();
    assert_eq!(factory.getClassInfo(0, &mut info), kResultOk);
    let cid = info.cid;
    assert_eq!(
        cid,
        derive_cid("test.vst3fixture"),
        "factory must advertise the derived cid"
    );

    let mut obj: *mut c_void = std::ptr::null_mut();
    let res = factory.createInstance(
        cid.as_ptr() as FIDString,
        IComponent::IID.as_ptr() as FIDString,
        &mut obj,
    );
    assert_eq!(res, kResultOk, "createInstance failed");
    assert!(!obj.is_null());

    ComPtr::from_raw(obj as *mut IComponent).unwrap()
}

#[test]
fn factory_rejects_unknown_class_id() {
    unsafe {
        let factory = ComWrapper::new(Vst3Factory::<Fixture>::default())
            .to_com_ptr::<IPluginFactory>()
            .unwrap();
        let wrong = derive_cid("some.other.plugin");
        let mut obj: *mut c_void = std::ptr::null_mut();
        let res = factory.createInstance(
            wrong.as_ptr() as FIDString,
            IComponent::IID.as_ptr() as FIDString,
            &mut obj,
        );
        assert_ne!(res, kResultOk, "factory must not build a foreign class id");
    }
}

#[test]
fn processes_audio_and_emits_events() {
    unsafe {
        let component = instantiate();
        assert_eq!(component.initialize(std::ptr::null_mut()), kResultOk);

        // Buses: stereo audio in/out, plus an event output because the
        // fixture reports emits_midi().
        assert_eq!(
            component.getBusCount(
                MediaTypes_::kAudio as MediaType,
                BusDirections_::kInput as BusDirection
            ),
            1
        );
        assert_eq!(
            component.getBusCount(
                MediaTypes_::kEvent as MediaType,
                BusDirections_::kOutput as BusDirection
            ),
            1,
            "a MIDI-emitting plugin must advertise an event output bus"
        );

        let processor = component
            .cast::<IAudioProcessor>()
            .expect("IAudioProcessor");
        let controller = component
            .cast::<IEditController>()
            .expect("IEditController");

        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kRealtime as int32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
            maxSamplesPerBlock: 512,
            sampleRate: 48_000.0,
        };
        assert_eq!(processor.setupProcessing(&mut setup), kResultOk);

        // Host sets gain to half scale (normalized 0.25 of 0..2 == 0.5).
        assert_eq!(controller.setParamNormalized(0, 0.25), kResultOk);
        assert_eq!(controller.getParamNormalized(0), 0.25);

        let mut left = [1.0f32; 64];
        let mut right = [1.0f32; 64];
        let mut out_ptrs = [left.as_mut_ptr(), right.as_mut_ptr()];
        let mut out_bus = AudioBusBuffers {
            numChannels: 2,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: out_ptrs.as_mut_ptr(),
            },
        };
        // The host connects an input; here it is the same memory, since
        // in-place processing must be handled anyway.
        let mut in_ptrs = out_ptrs;
        let mut in_bus = AudioBusBuffers {
            numChannels: 2,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: in_ptrs.as_mut_ptr(),
            },
        };

        let sink = ComWrapper::new(EventSink::default());
        let sink_ptr = sink.to_com_ptr::<IEventList>().unwrap();

        let mut data: ProcessData = std::mem::zeroed();
        data.numSamples = 64;
        data.numInputs = 1;
        data.inputs = &mut in_bus;
        data.numOutputs = 1;
        data.outputs = &mut out_bus;
        data.symbolicSampleSize = SymbolicSampleSizes_::kSample32 as int32;
        data.outputEvents = sink_ptr.as_ptr();

        assert_eq!(processor.process(&mut data), kResultOk);

        // Gain 0.5 applied to a buffer of ones.
        assert!(
            left.iter().all(|s| (s - 0.5).abs() < 1e-6),
            "audio was not processed: got {}",
            left[0]
        );
        assert!(right.iter().all(|s| (s - 0.5).abs() < 1e-6));

        // The generated MIDI arrived as a native VST3 note-on event.
        let events = sink_ptr.getEventCount();
        assert_eq!(events, 1, "expected one note event");
        let mut ev: Event = std::mem::zeroed();
        assert_eq!(sink_ptr.getEvent(0, &mut ev), kResultOk);
        assert_eq!(ev.r#type, Event_::EventTypes_::kNoteOnEvent as u16);
        assert_eq!(ev.__field0.noteOn.pitch, 64);
        assert!((ev.__field0.noteOn.velocity - 100.0 / 127.0).abs() < 1e-6);

        // With no input connected the output is silenced before processing,
        // not left holding whatever the buffers contained.
        left.fill(1.0);
        right.fill(1.0);
        data.numInputs = 0;
        data.inputs = std::ptr::null_mut();
        assert_eq!(processor.process(&mut data), kResultOk);
        assert!(
            left.iter().all(|s| *s == 0.0),
            "no input must render silence, got {}",
            left[0]
        );
        assert!(right.iter().all(|s| *s == 0.0));

        assert_eq!(component.terminate(), kResultOk);
    }
}

/// Minimal IParamValueQueue: a fixed list of (sampleOffset, value) points.
struct ParamQueue {
    id: ParamID,
    points: Vec<(int32, ParamValue)>,
}

impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id
    }
    unsafe fn getPointCount(&self) -> int32 {
        self.points.len() as int32
    }
    unsafe fn getPoint(
        &self,
        index: int32,
        sampleOffset: *mut int32,
        value: *mut ParamValue,
    ) -> tresult {
        match self.points.get(index as usize) {
            Some(&(off, val)) if !sampleOffset.is_null() && !value.is_null() => {
                *sampleOffset = off;
                *value = val;
                kResultTrue
            }
            _ => kInvalidArgument,
        }
    }
    unsafe fn addPoint(
        &self,
        _sampleOffset: int32,
        _value: ParamValue,
        _index: *mut int32,
    ) -> tresult {
        kInvalidArgument // the adapter never adds points
    }
}

/// Minimal IParameterChanges: one queue.
struct ParamChanges {
    queue: ComPtr<IParamValueQueue>,
}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        1
    }
    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        if index == 0 {
            self.queue.as_ptr()
        } else {
            std::ptr::null_mut()
        }
    }
    unsafe fn addParameterData(
        &self,
        _id: *const ParamID,
        _index: *mut int32,
    ) -> *mut IParamValueQueue {
        std::ptr::null_mut()
    }
}

/// Timed automation points must take effect at their sample offsets, not at
/// the top of the block: the adapter splits the block and renders segments.
#[test]
fn timed_param_points_split_the_block() {
    unsafe {
        let component = instantiate();
        assert_eq!(component.initialize(std::ptr::null_mut()), kResultOk);
        let processor = component
            .cast::<IAudioProcessor>()
            .expect("IAudioProcessor");

        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kRealtime as int32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
            maxSamplesPerBlock: 512,
            sampleRate: 48_000.0,
        };
        assert_eq!(processor.setupProcessing(&mut setup), kResultOk);

        // Out of order within the queue: gain 0.5 at frame 48 (normalized
        // 0.25 of 0..2), then gain 0.0 at frame 16. Default gain is 1.0.
        let queue = ComWrapper::new(ParamQueue {
            id: 0,
            points: vec![(48, 0.25), (16, 0.0)],
        });
        let queue_ptr = queue.to_com_ptr::<IParamValueQueue>().unwrap();
        let changes = ComWrapper::new(ParamChanges { queue: queue_ptr });
        let changes_ptr = changes.to_com_ptr::<IParameterChanges>().unwrap();

        let mut left = [1.0f32; 64];
        let mut right = [1.0f32; 64];
        let mut out_ptrs = [left.as_mut_ptr(), right.as_mut_ptr()];
        let mut out_bus = AudioBusBuffers {
            numChannels: 2,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: out_ptrs.as_mut_ptr(),
            },
        };
        let mut in_ptrs = out_ptrs;
        let mut in_bus = AudioBusBuffers {
            numChannels: 2,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: in_ptrs.as_mut_ptr(),
            },
        };

        let mut data: ProcessData = std::mem::zeroed();
        data.numSamples = 64;
        data.numInputs = 1;
        data.inputs = &mut in_bus;
        data.numOutputs = 1;
        data.outputs = &mut out_bus;
        data.symbolicSampleSize = SymbolicSampleSizes_::kSample32 as int32;
        data.inputParameterChanges = changes_ptr.as_ptr();

        assert_eq!(processor.process(&mut data), kResultOk);

        assert!(
            left[0..16].iter().all(|s| (s - 1.0).abs() < 1e-6),
            "default gain before frame 16, got {}",
            left[8]
        );
        assert!(
            left[16..48].iter().all(|s| s.abs() < 1e-6),
            "gain 0.0 from frame 16, got {}",
            left[32]
        );
        assert!(
            left[48..64].iter().all(|s| (s - 0.5).abs() < 1e-6),
            "gain 0.5 from frame 48, got {}",
            left[56]
        );

        // The final point's value must stick for the next block.
        let controller = component
            .cast::<IEditController>()
            .expect("IEditController");
        assert_eq!(controller.getParamNormalized(0), 0.25);

        assert_eq!(component.terminate(), kResultOk);
    }
}

#[test]
fn state_round_trips_through_a_stream() {
    unsafe {
        let a = instantiate();
        assert_eq!(a.initialize(std::ptr::null_mut()), kResultOk);
        let ctrl_a = a.cast::<IEditController>().unwrap();
        ctrl_a.setParamNormalized(0, 0.75);
        ctrl_a.setParamNormalized(1, 1.0); // last choice: "Auto"

        let stream = ComWrapper::new(MemStream::new(Vec::new()));
        let stream_ptr = stream.to_com_ptr::<IBStream>().unwrap();
        assert_eq!(a.getState(stream_ptr.as_ptr()), kResultOk);

        let saved = stream.data.borrow().clone();
        assert!(!saved.is_empty(), "getState wrote nothing");

        // Fresh instance, load the saved bytes back.
        let b = instantiate();
        assert_eq!(b.initialize(std::ptr::null_mut()), kResultOk);
        let reload = ComWrapper::new(MemStream::new(saved));
        let reload_ptr = reload.to_com_ptr::<IBStream>().unwrap();
        assert_eq!(b.setState(reload_ptr.as_ptr()), kResultOk);

        let ctrl_b = b.cast::<IEditController>().unwrap();
        assert!(
            (ctrl_b.getParamNormalized(0) - 0.75).abs() < 1e-9,
            "gain did not round-trip"
        );
        assert!(
            (ctrl_b.getParamNormalized(1) - 1.0).abs() < 1e-9,
            "choice did not round-trip"
        );
    }
}

#[test]
fn choice_parameters_report_as_stepped_lists() {
    unsafe {
        let component = instantiate();
        let controller = component.cast::<IEditController>().unwrap();
        assert_eq!(controller.getParameterCount(), 2);

        let mut info: ParameterInfo = std::mem::zeroed();
        assert_eq!(controller.getParameterInfo(1, &mut info), kResultOk);
        assert_eq!(info.id, 1);
        assert_eq!(info.stepCount, 2, "three choices means two steps");
        assert!(
            info.flags & ParameterInfo_::ParameterFlags_::kIsList as int32 != 0,
            "a choice parameter must be flagged as a list so hosts show a dropdown"
        );

        // Middle choice displays its label, and the label parses back.
        let mut text: String128 = [0; 128];
        assert_eq!(
            controller.getParamStringByValue(1, 0.5, &mut text),
            kResultOk
        );
        let shown = String::from_utf16_lossy(
            &text
                .iter()
                .take_while(|c| **c != 0)
                .copied()
                .collect::<Vec<_>>(),
        );
        assert_eq!(shown, "On");

        let mut back = -1.0f64;
        assert_eq!(
            controller.getParamValueByString(1, text.as_mut_ptr(), &mut back),
            kResultOk
        );
        assert!(
            (back - 0.5).abs() < 1e-9,
            "label did not parse back to its value"
        );
    }
}

/// Records what the host is told, so editor-driven edits can be checked.
#[derive(Default)]
struct RecordingHandler {
    edits: RefCell<Vec<(u32, f64)>>,
    gestures: RefCell<Vec<&'static str>>,
}

impl Class for RecordingHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for RecordingHandler {
    unsafe fn beginEdit(&self, _id: u32) -> tresult {
        self.gestures.borrow_mut().push("begin");
        kResultOk
    }
    unsafe fn performEdit(&self, id: u32, valueNormalized: f64) -> tresult {
        self.edits.borrow_mut().push((id, valueNormalized));
        self.gestures.borrow_mut().push("perform");
        kResultOk
    }
    unsafe fn endEdit(&self, _id: u32) -> tresult {
        self.gestures.borrow_mut().push("end");
        kResultOk
    }
    unsafe fn restartComponent(&self, _flags: int32) -> tresult {
        kResultOk
    }
}

#[test]
fn offers_an_editor_view_with_the_right_size() {
    unsafe {
        let component = instantiate();
        let controller = component.cast::<IEditController>().unwrap();

        let view = controller.createView(ViewType::kEditor);
        assert!(
            !view.is_null(),
            "a plugin with editor HTML must offer a view"
        );
        let view = ComPtr::<IPlugView>::from_raw(view).unwrap();

        // NSView is the platform we support; nonsense must be refused.
        assert_eq!(
            view.isPlatformTypeSupported(kPlatformTypeNSView),
            kResultTrue
        );
        assert_ne!(view.isPlatformTypeSupported(kPlatformTypeHWND), kResultTrue);

        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        assert_eq!(view.getSize(&mut rect), kResultOk);
        assert_eq!((rect.right - rect.left, rect.bottom - rect.top), (321, 123));

        // Fixed size: the host is pushed back to our dimensions.
        assert_eq!(view.canResize(), kResultFalse);
        let mut wrong = ViewRect {
            left: 0,
            top: 0,
            right: 999,
            bottom: 999,
        };
        assert_eq!(view.checkSizeConstraint(&mut wrong), kResultOk);
        assert_eq!((wrong.right, wrong.bottom), (321, 123));

        // Attaching to a bogus platform type must fail rather than crash.
        assert_ne!(
            view.attached(std::ptr::null_mut(), kPlatformTypeHWND),
            kResultOk
        );
        // removed() before a successful attach must be harmless.
        assert_eq!(view.removed(), kResultOk);

        // Input is left to the webview so host shortcuts keep working.
        assert_eq!(view.onWheel(1.0), kResultFalse);
        assert_eq!(view.onKeyDown(65, 0, 0), kResultFalse);
    }
}

#[test]
fn unnamed_view_is_refused() {
    unsafe {
        let component = instantiate();
        let controller = component.cast::<IEditController>().unwrap();
        let bogus = std::ffi::CString::new("not-an-editor").unwrap();
        let view = controller.createView(bogus.as_ptr());
        assert!(view.is_null(), "only the editor view exists");
    }
}

#[test]
fn editor_edits_reach_the_host_as_gestures() {
    unsafe {
        // Built directly rather than through the factory, so the test can
        // hold both the COM interface and the Rust object behind it.
        let plugin = ComWrapper::new(Vst3Plugin::<Fixture>::new());
        let controller = plugin.to_com_ptr::<IEditController>().unwrap();
        let component = plugin.to_com_ptr::<IComponent>().unwrap();
        assert_eq!(component.initialize(std::ptr::null_mut()), kResultOk);

        let handler = ComWrapper::new(RecordingHandler::default());
        let handler_ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();
        assert_eq!(
            controller.setComponentHandler(handler_ptr.as_ptr()),
            kResultOk
        );

        // Drive the path the webview's IPC handler uses. Gain spans 0..2,
        // so a plain value of 1.5 must reach the host normalized to 0.75.
        plugin.editor_edit(0, 1.5);

        assert_eq!(handler.edits.borrow().as_slice(), &[(0, 0.75)]);
        assert_eq!(
            handler.gestures.borrow().as_slice(),
            &["begin", "perform", "end"],
            "VST3 requires an edit gesture around every GUI change"
        );
        // The processor itself moved too, in plain units.
        assert_eq!(controller.getParamNormalized(0), 0.75);
    }
}
