//! skuiz-dsp: DSP integration for Skuiz plugins.
//!
//! # Writing DSP in C
//!
//! There is deliberately nothing here for it. A C DSP routine is reached
//! with a plain `extern "C"` block and built with the `cc` crate in the
//! plugin's own `build.rs` — a wrapper layer on top of that would be pure
//! ceremony. See `examples/trigger-note` for the whole pattern.
//!
//! # Embedding Pure Data
//!
//! `PdEngine` (feature `libpd`) is where this crate earns its place,
//! because embedding Pd in a plugin has two sharp edges that every host
//! runs straight into:
//!
//! - **Pd is a process-wide singleton by default.** Two plugin instances
//!   would share one patch, one DSP graph, and one set of receivers. Each
//!   `PdEngine` therefore owns a separate `pdinstance` and selects it
//!   before every call, so instances stay independent.
//! - **Pd only ever processes 64 frames at a time**, while hosts hand over
//!   whatever block size they like — 100 frames, or a different size every
//!   block. `PdEngine::process` adapts between the two, at the cost of
//!   `PdEngine::latency_frames` samples of delay.

#![warn(missing_docs)]

#[cfg(feature = "libpd")]
mod pd {

    use std::ffi::CString;
    use std::os::raw::c_void;
    use std::path::Path;
    use std::sync::{Mutex, Once, OnceLock};

    /// Serialises Pd setup. Per-instance *processing* runs lock-free (that is
    /// the point of separate `pdinstance`s), but creating instances, opening
    /// patches and freeing still touch process-wide Pd state.
    fn setup_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Why an engine could not be created or a patch could not be loaded.
    #[derive(Debug)]
    pub enum PdError {
        /// libpd was built without multi-instance support, so plugin instances
        /// would trample each other's patches.
        NoInstanceSupport,
        /// Pd refused the requested channel count or sample rate.
        AudioInit,
        /// The patch path had no usable file name or parent directory.
        PatchNotFound,
        /// Pd could not open the patch (missing file or parse error).
        PatchFailed,
    }

    impl std::fmt::Display for PdError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                PdError::NoInstanceSupport => {
                    write!(f, "libpd was built without multi-instance support")
                }
                PdError::AudioInit => write!(f, "Pd refused the audio configuration"),
                PdError::PatchNotFound => write!(f, "patch path has no file name or parent"),
                PdError::PatchFailed => write!(f, "Pd could not open the patch"),
            }
        }
    }

    impl std::error::Error for PdError {}

    /// An embedded Pure Data instance: one patch, one DSP graph, independent of
    /// every other `PdEngine` in the process.
    pub struct PdEngine {
        instance: *mut libpd_sys::_pdinstance,
        patch: *mut c_void,
        channels: usize,
        block: usize,
        /// One tick of interleaved input, filled a frame at a time.
        in_buf: Vec<f32>,
        in_fill: usize,
        /// One tick of interleaved output, straight from Pd.
        tick_out: Vec<f32>,
        /// Interleaved output ring absorbing the mismatch between Pd's fixed
        /// tick and the host's block size. Reads never overrun writes: the
        /// engine is primed with one tick of silence, and every further tick
        /// writes exactly the frames the reads that fed it will consume.
        ring: Vec<f32>,
        read: usize,
        write: usize,
    }

    // Safety: every method selects `instance` on the calling thread before
    // touching Pd, and the instance is owned solely by this engine.
    unsafe impl Send for PdEngine {}

    impl PdEngine {
        /// Create an engine with `channels` in and out. Call off the audio
        /// thread — this allocates and takes the global Pd setup lock.
        ///
        /// Input and output are one count: a generator patch cannot ask for
        /// 0 inputs, it just ignores the channels it gets.
        pub fn new(sample_rate: f64, channels: usize) -> Result<Self, PdError> {
            static INIT: Once = Once::new();
            let _guard = setup_lock().lock().unwrap_or_else(|e| e.into_inner());

            unsafe {
                INIT.call_once(|| {
                    libpd_sys::libpd_init();
                    // Pd chatters on stderr otherwise, which in a plugin means
                    // chattering into the host's log.
                    libpd_sys::libpd_set_verbose(0);
                });

                let instance = libpd_sys::libpd_new_instance();
                if instance.is_null() {
                    return Err(PdError::NoInstanceSupport);
                }
                libpd_sys::libpd_set_instance(instance);

                let channels = channels.max(1);
                if libpd_sys::libpd_init_audio(channels as i32, channels as i32, sample_rate as i32)
                    != 0
                {
                    libpd_sys::libpd_free_instance(instance);
                    return Err(PdError::AudioInit);
                }

                let block = libpd_sys::libpd_blocksize().max(1) as usize;
                let ring_frames = block * 2 + 1;
                let mut engine = Self {
                    instance,
                    patch: std::ptr::null_mut(),
                    channels,
                    block,
                    in_buf: vec![0.0; block * channels],
                    in_fill: 0,
                    tick_out: vec![0.0; block * channels],
                    ring: vec![0.0; ring_frames * channels],
                    read: 0,
                    write: 0,
                };
                // Prime one tick of silence so `process` always has output to
                // hand back, which is what makes the latency constant.
                engine.write = block;
                Ok(engine)
            }
        }

        /// Constant delay this engine adds, in frames. Report it to the host.
        pub fn latency_frames(&self) -> u32 {
            self.block as u32
        }

        /// Load a `.pd` patch and start its DSP. Call off the audio thread.
        pub fn open_patch(&mut self, path: &Path) -> Result<(), PdError> {
            let (Some(name), Some(dir)) = (path.file_name(), path.parent()) else {
                return Err(PdError::PatchNotFound);
            };
            // A bare file name has parent "" — mean the current directory,
            // or libpd searches its own paths instead of next to the patch.
            let dir = if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            };
            let (Ok(name), Ok(dir)) = (
                CString::new(name.to_string_lossy().as_bytes()),
                CString::new(dir.to_string_lossy().as_bytes()),
            ) else {
                return Err(PdError::PatchNotFound);
            };

            let _guard = setup_lock().lock().unwrap_or_else(|e| e.into_inner());
            unsafe {
                libpd_sys::libpd_set_instance(self.instance);
                self.close_patch_locked();

                let handle = libpd_sys::libpd_openfile(name.as_ptr(), dir.as_ptr());
                if handle.is_null() {
                    return Err(PdError::PatchFailed);
                }
                self.patch = handle;
                self.set_dsp(true);
            }
            Ok(())
        }

        /// Send a float to a `[receive]` name in the patch — the usual way to
        /// drive a patch from plugin parameters.
        pub fn send_float(&mut self, receiver: &str, value: f32) {
            let Ok(name) = CString::new(receiver) else {
                return;
            };
            unsafe {
                libpd_sys::libpd_set_instance(self.instance);
                // Fire-and-forget: the return only flags a receiver nobody
                // is bound to, which a parameter stream can't act on.
                libpd_sys::libpd_float(name.as_ptr(), value);
            }
        }

        /// Run one block. `channels` is deinterleaved and processed in place;
        /// slices may be any length, including lengths Pd cannot process
        /// directly. Unequal slice lengths process up to the shortest rather
        /// than panic. Realtime-safe: no allocation, no global lock.
        pub fn process(&mut self, channels: &mut [&mut [f32]]) {
            let frames = channels.iter().map(|c| c.len()).min().unwrap_or(0);
            if frames == 0 {
                return;
            }
            unsafe { libpd_sys::libpd_set_instance(self.instance) };

            for frame in 0..frames {
                for ch in 0..self.channels {
                    let sample = channels.get(ch).map_or(0.0, |c| c[frame]);
                    self.in_buf[self.in_fill * self.channels + ch] = sample;
                }
                self.in_fill += 1;
                if self.in_fill == self.block {
                    self.in_fill = 0;
                    self.run_tick();
                }

                for ch in 0..self.channels {
                    let sample = self.ring[self.read * self.channels + ch];
                    if let Some(out) = channels.get_mut(ch) {
                        out[frame] = sample;
                    }
                }
                self.read = (self.read + 1) % (self.ring.len() / self.channels);
            }
        }

        /// Process one Pd tick and push its output into the ring.
        fn run_tick(&mut self) {
            unsafe {
                // Return value is Pd's DSP error flag; there is nothing
                // realtime-safe to do with it here.
                libpd_sys::libpd_process_float(1, self.in_buf.as_ptr(), self.tick_out.as_mut_ptr());
            }
            let ring_frames = self.ring.len() / self.channels;
            for frame in 0..self.block {
                for ch in 0..self.channels {
                    self.ring[self.write * self.channels + ch] =
                        self.tick_out[frame * self.channels + ch];
                }
                self.write = (self.write + 1) % ring_frames;
            }
        }

        /// Caller must hold the setup lock and have selected this instance.
        unsafe fn set_dsp(&self, on: bool) {
            let Ok(pd) = CString::new("pd") else { return };
            let Ok(dsp) = CString::new("dsp") else { return };
            // Fire-and-forget, like `send_float`: a failed message send
            // leaves DSP off, which fails safe (silence).
            libpd_sys::libpd_start_message(1);
            libpd_sys::libpd_add_float(if on { 1.0 } else { 0.0 });
            libpd_sys::libpd_finish_message(pd.as_ptr(), dsp.as_ptr());
        }

        /// Caller must hold the setup lock and have selected this instance.
        unsafe fn close_patch_locked(&mut self) {
            if !self.patch.is_null() {
                libpd_sys::libpd_closefile(self.patch);
                self.patch = std::ptr::null_mut();
            }
        }
    }

    impl Drop for PdEngine {
        fn drop(&mut self) {
            let _guard = setup_lock().lock().unwrap_or_else(|e| e.into_inner());
            unsafe {
                libpd_sys::libpd_set_instance(self.instance);
                self.set_dsp(false);
                self.close_patch_locked();
                libpd_sys::libpd_free_instance(self.instance);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Writes a patch that halves its input, so a known input gives a
        /// known output and the test can tell real DSP from silence.
        fn halving_patch(dir: &Path) -> std::path::PathBuf {
            let path = dir.join("half.pd");
            std::fs::write(
                &path,
                "#N canvas 0 0 450 300 12;\n\
                 #X obj 50 50 adc~;\n\
                 #X obj 50 100 *~ 0.5;\n\
                 #X obj 50 150 dac~;\n\
                 #X connect 0 0 1 0;\n\
                 #X connect 1 0 2 0;\n",
            )
            .unwrap();
            path
        }

        /// Stereo version of `halving_patch`: each channel halved on its
        /// own, so swapped or duplicated interleaving shows up.
        fn stereo_halving_patch(dir: &Path) -> std::path::PathBuf {
            let path = dir.join("half_stereo.pd");
            std::fs::write(
                &path,
                "#N canvas 0 0 450 300 12;\n\
                 #X obj 50 50 adc~;\n\
                 #X obj 50 100 *~ 0.5;\n\
                 #X obj 150 100 *~ 0.5;\n\
                 #X obj 50 150 dac~;\n\
                 #X connect 0 0 1 0;\n\
                 #X connect 0 1 2 0;\n\
                 #X connect 1 0 3 0;\n\
                 #X connect 2 0 3 1;\n",
            )
            .unwrap();
            path
        }

        fn scratch() -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!("skuiz-pd-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn patch_processes_audio_at_any_block_size() {
            let dir = scratch();
            let patch = halving_patch(&dir);

            let mut pd = PdEngine::new(48_000.0, 1).expect("pd engine");
            pd.open_patch(&patch).expect("open patch");
            let latency = pd.latency_frames() as usize;

            // Deliberately not a multiple of Pd's 64-frame tick: this is the
            // case that naive libpd integrations get wrong.
            let block = 100;
            let mut produced = Vec::new();
            for _ in 0..12 {
                let mut buf = vec![1.0f32; block];
                let mut chans: [&mut [f32]; 1] = [&mut buf];
                pd.process(&mut chans);
                produced.extend_from_slice(&buf);
            }

            // Past the priming latency the patch should be halving its input.
            let steady = &produced[latency + 64..];
            assert!(!steady.is_empty());
            for (i, s) in steady.iter().enumerate() {
                assert!(
                    (s - 0.5).abs() < 1e-6,
                    "sample {i} was {s}, expected 0.5 from [*~ 0.5]"
                );
            }
        }

        #[test]
        fn instances_are_independent() {
            let dir = scratch();
            let patch = halving_patch(&dir);

            // Only `a` gets a patch; `b` must stay silent rather than sharing
            // a's DSP graph, which is what a process-wide Pd would do.
            let mut a = PdEngine::new(48_000.0, 1).expect("engine a");
            let mut b = PdEngine::new(48_000.0, 1).expect("engine b");
            a.open_patch(&patch).expect("open patch");

            let mut out_a = Vec::new();
            let mut out_b = Vec::new();
            for _ in 0..8 {
                let mut buf_a = vec![1.0f32; 128];
                let mut buf_b = vec![1.0f32; 128];
                a.process(&mut [&mut buf_a]);
                b.process(&mut [&mut buf_b]);
                out_a.extend_from_slice(&buf_a);
                out_b.extend_from_slice(&buf_b);
            }

            assert!(
                out_a.iter().any(|s| (s - 0.5).abs() < 1e-6),
                "engine with the patch produced no signal"
            );
            assert!(
                out_b.iter().all(|s| s.abs() < 1e-6),
                "engine without a patch produced signal: instances are sharing state"
            );
        }

        /// Distinct L/R constants in, halved and unswapped out: proves the
        /// deinterleave/interleave mapping, which mono tests cannot see.
        #[test]
        fn stereo_channels_interleave_in_order() {
            let dir = scratch();
            let patch = stereo_halving_patch(&dir);

            let mut pd = PdEngine::new(48_000.0, 2).expect("pd engine");
            pd.open_patch(&patch).expect("open patch");
            let latency = pd.latency_frames() as usize;

            let block = 100;
            let mut left = Vec::new();
            let mut right = Vec::new();
            for _ in 0..12 {
                let mut l = vec![1.0f32; block];
                let mut r = vec![0.25f32; block];
                pd.process(&mut [&mut l, &mut r]);
                left.extend_from_slice(&l);
                right.extend_from_slice(&r);
            }

            let steady_l = &left[latency + 64..];
            let steady_r = &right[latency + 64..];
            assert!(!steady_l.is_empty());
            for (i, (l, r)) in steady_l.iter().zip(steady_r).enumerate() {
                assert!(
                    (l - 0.5).abs() < 1e-6,
                    "left sample {i} was {l}, expected 0.5"
                );
                assert!(
                    (r - 0.125).abs() < 1e-6,
                    "right sample {i} was {r}, expected 0.125 — swapped or duplicated channels?"
                );
            }
        }
    }
}

#[cfg(feature = "libpd")]
pub use pd::*;
