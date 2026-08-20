//! RT-hammer: hostile concurrency against the engine's threading model.
//! One instance is activated and processing, then attacked from two sides:
//! an audio thread pumping process() and the main thread hammering
//! setParamNormalized/getParamNormalized plus getState/setState through an
//! in-memory stream. The engine must survive without deadlock or panic,
//! and the final value must be one that was actually sent.

#![allow(non_snake_case)]

use skuiz_core::{ParamDef, PluginInfo, Processor};
use skuiz_vst3::vst3::Steinberg::Vst::*;
use skuiz_vst3::vst3::Steinberg::*;
use skuiz_vst3::vst3::{Class, ComPtr, ComRef, ComWrapper, Interface};
use skuiz_vst3::{derive_cid, Vst3Factory};
use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Gain(f64);

impl Default for Gain {
    fn default() -> Self {
        Self(1.0)
    }
}

impl Processor for Gain {
    fn info() -> PluginInfo {
        // Distinct id => distinct bus socket; keeps other tests out.
        PluginInfo {
            id: "test.vst3hammer",
            name: "g",
            vendor: "t",
            version: "0",
            description: "",
        }
    }
    fn params() -> &'static [ParamDef] {
        &[ParamDef {
            id: 0,
            name: "Gain",
            min: 0.0,
            max: 1.0,
            default: 1.0,
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
    fn process(&mut self, channels: &mut [&mut [f32]], _midi: &mut skuiz_core::MidiOut) {
        let g = self.0 as f32;
        for ch in channels.iter_mut() {
            for s in ch.iter_mut() {
                *s *= g;
            }
        }
    }
}

/// Every value the main thread ever sends, normalized (the 0..1 range makes
/// plain == normalized). The default (1.0) is a candidate too, so "the
/// mirror holds a candidate" holds even before anything lands.
fn candidate(i: usize) -> f64 {
    (i % 8) as f64 / 7.0
}

fn is_candidate(v: f64) -> bool {
    (0..8).any(|k| (v - candidate(k)).abs() < 1e-12)
}

/// COM pointers are not Send; this newtype moves one to the single audio
/// thread. Sound here: only that thread calls process(); the main thread
/// sticks to main-thread-safe entry points (controller calls, state ops).
struct SendPtr<T>(T);
unsafe impl<T> Send for SendPtr<T> {}

impl<T> SendPtr<T> {
    /// A method, not a field access or destructure, so closures capture the
    /// Send wrapper whole instead of the raw pointer field inside it.
    fn into_inner(self) -> T {
        self.0
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

/// Host stand-in. `getParamNormalized` may call `restartComponent` when
/// remote changes landed; it must be tolerated, like any host would.
struct MockHandler;

impl Class for MockHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for MockHandler {
    unsafe fn beginEdit(&self, _id: u32) -> tresult {
        kResultOk
    }
    unsafe fn performEdit(&self, _id: u32, _valueNormalized: f64) -> tresult {
        kResultOk
    }
    unsafe fn endEdit(&self, _id: u32) -> tresult {
        kResultOk
    }
    unsafe fn restartComponent(&self, _flags: int32) -> tresult {
        kResultOk
    }
}

/// Create a plugin instance through the factory, exactly as a host would.
unsafe fn instantiate() -> ComPtr<IComponent> {
    let factory = ComWrapper::new(Vst3Factory::<Gain>::default())
        .to_com_ptr::<IPluginFactory>()
        .unwrap();

    let mut info: PClassInfo = std::mem::zeroed();
    assert_eq!(factory.getClassInfo(0, &mut info), kResultOk);
    let cid = info.cid;
    assert_eq!(cid, derive_cid("test.vst3hammer"));

    let mut obj: *mut c_void = std::ptr::null_mut();
    let res = factory.createInstance(
        cid.as_ptr() as FIDString,
        IComponent::IID.as_ptr() as FIDString,
        &mut obj,
    );
    assert_eq!(res, kResultOk, "createInstance failed");
    ComPtr::from_raw(obj as *mut IComponent).unwrap()
}

/// Poll `cond` rather than sleep a fixed time: the suite runs in parallel,
/// so fixed delays are either flaky under load or needlessly slow.
fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

/// Sets `stop` even if the main thread panics mid-hammer, so the audio
/// thread winds down instead of spinning to its iteration cap.
struct StopGuard(Arc<AtomicBool>);
impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[test]
fn rt_hammer() {
    unsafe {
        let component = instantiate();
        assert_eq!(component.initialize(std::ptr::null_mut()), kResultOk);
        let processor = component
            .cast::<IAudioProcessor>()
            .expect("IAudioProcessor");
        let controller = component
            .cast::<IEditController>()
            .expect("IEditController");

        let handler = ComWrapper::new(MockHandler);
        let handler_ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();
        assert_eq!(
            controller.setComponentHandler(handler_ptr.as_ptr()),
            kResultOk
        );

        assert_eq!(component.setActive(1), kResultOk);
        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kRealtime as int32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
            maxSamplesPerBlock: 512,
            sampleRate: 48_000.0,
        };
        assert_eq!(processor.setupProcessing(&mut setup), kResultOk);
        assert_eq!(processor.setProcessing(1), kResultOk);

        let stop = Arc::new(AtomicBool::new(false));
        let _guard = StopGuard(Arc::clone(&stop));
        let audio_done = Arc::new(AtomicBool::new(false));

        // Thread A (audio): pump blocks until the main thread says stop.
        // The cap is a backstop so a failed test still terminates.
        let audio = {
            let processor = SendPtr(processor.as_ptr());
            let stop = Arc::clone(&stop);
            let audio_done = Arc::clone(&audio_done);
            std::thread::spawn(move || {
                let processor =
                    ComRef::<IAudioProcessor>::from_raw_unchecked(processor.into_inner());
                let mut left = [1.0f32; 64];
                let mut right = [1.0f32; 64];
                for _ in 0..500_000 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let mut out_ptrs = [left.as_mut_ptr(), right.as_mut_ptr()];
                    let mut out_bus = AudioBusBuffers {
                        numChannels: 2,
                        silenceFlags: 0,
                        __field0: AudioBusBuffers__type0 {
                            channelBuffers32: out_ptrs.as_mut_ptr(),
                        },
                    };
                    let mut data: ProcessData = std::mem::zeroed();
                    data.numSamples = 64;
                    data.numOutputs = 1;
                    data.outputs = &mut out_bus;
                    data.symbolicSampleSize = SymbolicSampleSizes_::kSample32 as int32;
                    assert_eq!(processor.process(&mut data), kResultOk);
                }
                audio_done.store(true, Ordering::Relaxed);
            })
        };

        // Main thread: normalized set/get, and a state round-trip every few
        // iterations. State ops while running are a bounded round-trip
        // through the audio thread — they only complete because thread A
        // keeps the blocks flowing.
        for i in 0..300 {
            assert_eq!(controller.setParamNormalized(0, candidate(i)), kResultOk);
            let v = controller.getParamNormalized(0);
            assert!(
                v.is_finite() && (0.0..=1.0).contains(&v),
                "mirror read out of range: {v}"
            );
            if i % 15 == 0 {
                let stream = ComWrapper::new(MemStream::new(Vec::new()));
                let stream_ptr = stream.to_com_ptr::<IBStream>().unwrap();
                assert_eq!(
                    component.getState(stream_ptr.as_ptr()),
                    kResultOk,
                    "getState failed while blocks were flowing"
                );
                let saved = stream.data.borrow().clone();
                assert!(!saved.is_empty(), "getState wrote nothing");
                let reload = ComWrapper::new(MemStream::new(saved));
                let reload_ptr = reload.to_com_ptr::<IBStream>().unwrap();
                assert_eq!(
                    component.setState(reload_ptr.as_ptr()),
                    kResultOk,
                    "setState failed while blocks were flowing"
                );
            }
        }

        // The audio thread must finish; join before asserting so it is
        // reaped (and its panics propagated) even on the failure path.
        stop.store(true, Ordering::Relaxed);
        wait_until("audio thread to finish", || {
            audio_done.load(Ordering::Relaxed)
        });
        audio.join().unwrap();

        // Queued commands may still be in flight; pump a few final blocks
        // (single-threaded now) so the mirror reflects the last applied one.
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
        let mut data: ProcessData = std::mem::zeroed();
        data.numSamples = 64;
        data.numOutputs = 1;
        data.outputs = &mut out_bus;
        data.symbolicSampleSize = SymbolicSampleSizes_::kSample32 as int32;
        for _ in 0..8 {
            assert_eq!(processor.process(&mut data), kResultOk);
        }
        wait_until("param to converge to a sent value", || {
            is_candidate(controller.getParamNormalized(0))
        });

        assert_eq!(processor.setProcessing(0), kResultOk);
        assert_eq!(component.setActive(0), kResultOk);
        assert_eq!(component.terminate(), kResultOk);
    }
}
