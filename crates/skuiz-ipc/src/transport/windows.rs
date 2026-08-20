//! Windows transport: a named pipe.
//!
//! Named pipes make the election simpler than on Unix rather than harder.
//! `FILE_FLAG_FIRST_PIPE_INSTANCE` fails if the name already exists, so
//! creating the pipe *is* the election, and because the name is a kernel
//! object owned by the creating process it disappears when that process
//! dies. There is no stale-name problem and so no equivalent of the Unix
//! `flock` is needed.
//!
//! Pipes are byte mode, giving the same stream semantics as a Unix socket,
//! so the length-prefixed framing above this layer is identical on both
//! platforms.
//!
//! All I/O is overlapped (`FILE_FLAG_OVERLAPPED`). This is not optional
//! here: a handle opened for synchronous I/O serializes operations, so a
//! `WriteFile` queues behind a pending `ReadFile` *on the same handle* —
//! with a reader thread and a writer sharing one duplex handle, both sides
//! of every connection deadlock the moment real traffic flows. Overlapped
//! operations carry their own `OVERLAPPED` and are not serialized, and as a
//! bonus `CancelIoEx` (which only cancels overlapped I/O) genuinely unblocks
//! a parked reader, and writes can carry the same timeout the Unix backend
//! gets from `SO_SNDTIMEO`.

use std::io::{Error, ErrorKind, Read, Result, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ,
    GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

const PIPE_BUFFER: u32 = 64 * 1024;

/// Bounds how long a write to a stuck (but not yet dead) peer can stall the
/// bus, matching the Unix backend; on expiry the write fails and the
/// connection is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// Wide, NUL-terminated string for the Win32 `W` entry points.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// An owned pipe handle, closed exactly once.
struct OwnedHandle(HANDLE);

// Safety: a pipe handle is usable from any thread, and this type is the sole
// owner of the one it holds.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A manual-reset event plus the `OVERLAPPED` that references it, scoped to
/// one I/O call. Overlapped operations on one handle each need their own
/// `OVERLAPPED`, and it must stay valid until the operation completes — so
/// every call site waits (or cancels and waits) before this drops.
struct Op {
    overlapped: OVERLAPPED,
}

impl Op {
    fn new() -> Option<Self> {
        // Safety: null attributes/name, manual reset, initially unsignalled.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return None;
        }
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event;
        Some(Self { overlapped })
    }

    /// Wait for the operation to finish (or until `timeout`), then harvest
    /// the byte count. On timeout the operation is cancelled and the cancel
    /// is awaited, so the stack `OVERLAPPED` is never touched after return.
    /// Safety: `handle` must be the handle the operation was issued on.
    unsafe fn finish(&mut self, handle: HANDLE, timeout: u32) -> Result<u32> {
        let waited = unsafe { WaitForSingleObject(self.overlapped.hEvent, timeout) };
        if waited != WAIT_OBJECT_0 {
            unsafe {
                CancelIoEx(handle, &self.overlapped);
                // Wait out the cancellation: the driver must be done with
                // `overlapped` before it goes out of scope.
                WaitForSingleObject(self.overlapped.hEvent, INFINITE);
            }
            return Err(Error::new(ErrorKind::TimedOut, "pipe I/O timed out"));
        }
        let mut done: u32 = 0;
        // Safety: the operation has completed; harvesting its result.
        let ok = unsafe { GetOverlappedResult(handle, &self.overlapped, &mut done, 0) };
        if ok == 0 {
            return Err(Error::last_os_error());
        }
        Ok(done)
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        if !self.overlapped.hEvent.is_null() {
            unsafe { CloseHandle(self.overlapped.hEvent) };
        }
    }
}

/// Where one scope's pipe lives.
///
/// Windows pipe names are a flat global namespace with no directories, so
/// the directory that separates buses on Unix is folded into the name.
pub(crate) struct Endpoint {
    name: Vec<u16>,
}

impl Endpoint {
    pub(crate) fn new(dir: &Path, scope: &str) -> Self {
        // Fold the directory into the name so `join_in` still isolates buses.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in dir.to_string_lossy().as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self {
            name: wide(&format!(r"\\.\pipe\skuiz-{hash:016x}-{scope}")),
        }
    }
}

/// A connection to one peer process.
pub(crate) struct Conn {
    /// Shared with the reader clone, not duplicated: overlapped operations
    /// on one handle each carry their own `OVERLAPPED` and do not serialize,
    /// so a blocked read no longer stalls a concurrent write (and vice
    /// versa). `CancelIoEx` cancels any of them from any thread.
    handle: Arc<OwnedHandle>,
    /// Serialises writes: a frame must not interleave with another's.
    write_lock: Mutex<()>,
}

impl Conn {
    fn new(handle: HANDLE) -> Self {
        Self {
            handle: Arc::new(OwnedHandle(handle)),
            write_lock: Mutex::new(()),
        }
    }

    /// A second reference to the same pipe for the reader thread. Sharing
    /// one handle is safe because all I/O is overlapped (module docs); the
    /// handle closes when the last clone drops, and `close` cancels the
    /// reader's blocked I/O.
    pub(crate) fn try_clone(&self) -> Option<Conn> {
        Some(Conn {
            handle: Arc::clone(&self.handle),
            write_lock: Mutex::new(()),
        })
    }

    /// Cancel any blocked I/O so a reader wakes up promptly.
    ///
    /// `CancelIoEx` rather than closing the handle: closing a handle another
    /// thread is blocked on is not safe on Windows, whereas cancelling is the
    /// documented way to unblock it — and because all I/O here is overlapped,
    /// the cancel is actually honoured.
    pub(crate) fn close(&self) {
        unsafe { CancelIoEx(self.handle.0, std::ptr::null()) };
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        debug_assert!(u32::try_from(buf.len()).is_ok());
        let mut op = Op::new().ok_or_else(Error::last_os_error)?;
        // Safety: `buf` is valid for `buf.len()` bytes until `op.finish`
        // reports the operation complete (or cancels and waits it out).
        // Reader threads wait without a deadline; `close` unblocks them.
        let ok = unsafe {
            ReadFile(
                self.handle.0,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                std::ptr::null_mut(),
                &mut op.overlapped,
            )
        };
        let read = if ok == 0 {
            let err = Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(err);
            }
            unsafe { op.finish(self.handle.0, INFINITE)? }
        } else {
            // Completed inline; the count still comes from the result call.
            unsafe { op.finish(self.handle.0, 0)? }
        };
        if read == 0 {
            // The peer closed: report EOF the way a socket would.
            return Err(Error::new(ErrorKind::UnexpectedEof, "pipe closed"));
        }
        Ok(read as usize)
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        debug_assert!(u32::try_from(buf.len()).is_ok());
        let mut op = Op::new().ok_or_else(Error::last_os_error)?;
        // Safety: `buf` is valid for `buf.len()` bytes until `op.finish`
        // reports the operation complete (or cancels and waits it out). The
        // timeout bounds how long a stuck peer can stall the bus lock above.
        let ok = unsafe {
            WriteFile(
                self.handle.0,
                buf.as_ptr().cast(),
                buf.len() as u32,
                std::ptr::null_mut(),
                &mut op.overlapped,
            )
        };
        let written = if ok == 0 {
            let err = Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(err);
            }
            let ms = u32::try_from(WRITE_TIMEOUT.as_millis()).unwrap_or(u32::MAX);
            unsafe { op.finish(self.handle.0, ms)? }
        } else {
            unsafe { op.finish(self.handle.0, 0)? }
        };
        Ok(written as usize)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Create one pipe instance. The first instance claims the name.
fn create_instance(endpoint: &Endpoint, first: bool) -> Option<HANDLE> {
    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // Safety: `name` is NUL-terminated and lives for the call.
    let handle = unsafe {
        CreateNamedPipeW(
            endpoint.name.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER,
            PIPE_BUFFER,
            0,
            std::ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        None
    } else {
        Some(handle)
    }
}

/// The server side. Holding the first instance holds the election.
pub(crate) struct Listener {
    endpoint_name: Vec<u16>,
    /// The instance currently waiting for a client.
    pending: Mutex<Option<OwnedHandle>>,
}

impl Listener {
    /// Block until a peer connects. `None` means the listener is finished.
    pub(crate) fn accept(&self) -> Option<Conn> {
        let mut slot = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let waiting = slot.take()?;

        // Overlapped, because the handle is overlapped: the call pends and
        // completes when a client arrives (or, with ERROR_PIPE_CONNECTED,
        // when one arrived before we called). `wake_listener` connects a
        // throwaway client to end the wait, so no deadline is needed here.
        let mut op = Op::new()?;
        // Safety: `waiting` is a valid pipe instance owned by us, and `op`
        // outlives the wait below.
        let connected = unsafe { ConnectNamedPipe(waiting.0, &mut op.overlapped) };
        if connected == 0 {
            let err = Error::last_os_error().raw_os_error().unwrap_or(0);
            if err == ERROR_IO_PENDING as i32 {
                // Safety: waits out or cancels the pending connect.
                if unsafe { op.finish(waiting.0, INFINITE) }.is_err() {
                    return None;
                }
            } else if err != ERROR_PIPE_CONNECTED as i32 {
                return None;
            }
        }

        // Replace the listening instance *before* handing this one over, so
        // the name never lapses and the election is never lost mid-accept.
        let endpoint = Endpoint {
            name: self.endpoint_name.clone(),
        };
        // Creating the next instance can fail transiently (handle
        // exhaustion), so retry briefly rather than ending the listener.
        // ponytail: if it keeps failing the listener still ends, and because
        // the live client connections continue to hold the name, this
        // process can neither serve again nor connect as a client until they
        // drain — recovering from that needs a fuller redesign.
        let mut next = None;
        for _ in 0..20 {
            if let Some(handle) = create_instance(&endpoint, false) {
                next = Some(handle);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        *slot = Some(OwnedHandle(next?));

        let handle = waiting.0;
        std::mem::forget(waiting); // ownership moves into the Conn
        Some(Conn::new(handle))
    }
}

/// Try to win the election for `endpoint`.
pub(crate) fn try_become_server(endpoint: &Endpoint) -> Option<Listener> {
    // Fails if the name exists, which is exactly the election. The name is
    // released by the kernel if the owning process dies, so unlike a Unix
    // socket path it can never be left stale.
    let first = create_instance(endpoint, true)?;
    Some(Listener {
        endpoint_name: endpoint.name.clone(),
        pending: Mutex::new(Some(OwnedHandle(first))),
    })
}

/// Connect to whichever process won the election.
pub(crate) fn connect(endpoint: &Endpoint) -> Option<Conn> {
    for _ in 0..2 {
        // Safety: `name` is NUL-terminated and lives for the call. Overlapped
        // like the server side, so our reads and writes never serialize.
        let handle = unsafe {
            CreateFileW(
                endpoint.name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Some(Conn::new(handle));
        }
        if Error::last_os_error().raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
            return None;
        }
        // Every instance is busy: wait for the server to free one.
        unsafe { WaitNamedPipeW(endpoint.name.as_ptr(), 200) };
    }
    None
}

/// Unblock a thread sitting in [`Listener::accept`].
pub(crate) fn wake_listener(endpoint: &Endpoint) {
    if let Some(conn) = connect(endpoint) {
        drop(conn);
    }
}

/// Nothing to clean up: the kernel frees the name with the last handle.
pub(crate) fn release(_endpoint: &Endpoint) {}
