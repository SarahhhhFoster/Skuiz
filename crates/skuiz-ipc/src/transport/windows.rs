//! Windows transport: a named pipe.
//!
//! **Unverified at runtime.** This compiles and type-checks for
//! `x86_64-pc-windows-msvc`, but it has not been executed — the project has
//! no Windows machine to run it on. Treat it as a starting point that needs
//! a real test pass, not as proven code.
//!
//! Named pipes make the election simpler than on Unix rather than harder.
//! `FILE_FLAG_FIRST_PIPE_INSTANCE` fails if the name already exists, so
//! creating the pipe *is* the election, and because the name is a kernel
//! object owned by the creating process it disappears when that process
//! dies. There is no stale-name problem and so no equivalent of the Unix
//! `flock` is needed.
//!
//! Pipes are byte mode and blocking, giving the same stream semantics as a
//! Unix socket, so the length-prefixed framing above this layer is identical
//! on both platforms.

use std::io::{Error, ErrorKind, Read, Result, Write};
use std::path::Path;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, ERROR_PIPE_BUSY, GENERIC_READ,
    GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::IO::CancelIoEx;

const PIPE_BUFFER: u32 = 64 * 1024;

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
    handle: OwnedHandle,
    /// Serialises writes: a frame must not interleave with another's.
    write_lock: Mutex<()>,
}

impl Conn {
    fn new(handle: HANDLE) -> Self {
        Self {
            handle: OwnedHandle(handle),
            write_lock: Mutex::new(()),
        }
    }

    pub(crate) fn try_clone(&self) -> Option<Conn> {
        let mut duplicate: HANDLE = std::ptr::null_mut();
        // Safety: both handles belong to this process; the source outlives
        // the call, and the duplicate is owned by the returned Conn.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                self.handle.0,
                GetCurrentProcess(),
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return None;
        }
        Some(Conn::new(duplicate))
    }

    /// Cancel any blocked I/O so a reader wakes up promptly.
    ///
    /// `CancelIoEx` rather than closing the handle: closing a handle another
    /// thread is blocked on is not safe on Windows, whereas cancelling is
    /// the documented way to unblock it.
    pub(crate) fn close(&self) {
        unsafe { CancelIoEx(self.handle.0, std::ptr::null()) };
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut read: u32 = 0;
        // Safety: `buf` is valid for `buf.len()` bytes for the call's duration.
        let ok = unsafe {
            ReadFile(
                self.handle.0,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(Error::last_os_error());
        }
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
        let mut written: u32 = 0;
        // Safety: `buf` is valid for `buf.len()` bytes for the call's duration.
        let ok = unsafe {
            WriteFile(
                self.handle.0,
                buf.as_ptr(),
                buf.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(Error::last_os_error());
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Create one pipe instance. The first instance claims the name.
fn create_instance(endpoint: &Endpoint, first: bool) -> Option<HANDLE> {
    let mut open_mode = PIPE_ACCESS_DUPLEX;
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

        // Safety: the handle is a valid pipe instance owned by `waiting`.
        let connected = unsafe { ConnectNamedPipe(waiting.0, std::ptr::null_mut()) };
        if connected == 0 {
            // ERROR_PIPE_CONNECTED means a client arrived first, which is
            // success; anything else ends the listener.
            let err = Error::last_os_error().raw_os_error().unwrap_or(0);
            const ERROR_PIPE_CONNECTED: i32 = 535;
            if err != ERROR_PIPE_CONNECTED {
                return None;
            }
        }

        // Replace the listening instance *before* handing this one over, so
        // the name never lapses and the election is never lost mid-accept.
        let endpoint = Endpoint {
            name: self.endpoint_name.clone(),
        };
        let next = create_instance(&endpoint, false)?;
        *slot = Some(OwnedHandle(next));

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
        // Safety: `name` is NUL-terminated and lives for the call.
        let handle = unsafe {
            CreateFileW(
                endpoint.name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
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
