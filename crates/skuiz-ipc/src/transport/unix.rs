//! Unix transport: a domain socket plus an `flock` election.
//!
//! Binding the socket cannot be the election on its own, because a Unix
//! socket file outlives the process that bound it: after a crash every
//! process sees a path that refuses connections, and if they each clear it,
//! one can unlink the socket another has just bound — leaving a server
//! listening on an unreachable path while everyone else fails to connect,
//! forever. An `flock` has no such failure mode, since the kernel releases
//! it when the holder dies. (Windows named pipes need no equivalent: the
//! name is a kernel object that disappears with its owner.)

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// Where one scope's socket and lock live.
pub(crate) struct Endpoint {
    sock_path: PathBuf,
    lock_path: PathBuf,
}

impl Endpoint {
    pub(crate) fn new(dir: &Path, scope: &str) -> Self {
        Self {
            sock_path: dir.join(format!("skuiz-{scope}.sock")),
            lock_path: dir.join(format!("skuiz-{scope}.lock")),
        }
    }
}

/// A connection to one peer process.
pub(crate) struct Conn(UnixStream);

impl Conn {
    pub(crate) fn try_clone(&self) -> Option<Conn> {
        self.0.try_clone().ok().map(Conn)
    }

    /// Close both directions, which unblocks a reader on either end at once.
    pub(crate) fn close(&self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// The server side: holds the election lock for as long as it exists.
pub(crate) struct Listener {
    inner: UnixListener,
    /// Dropping this releases the `flock`.
    _lock: File,
}

impl Listener {
    /// Block until a peer connects. `None` means the listener is finished.
    pub(crate) fn accept(&self) -> Option<Conn> {
        self.inner.accept().ok().map(|(stream, _)| Conn(stream))
    }
}

/// Try to win the election for `endpoint`.
pub(crate) fn try_become_server(endpoint: &Endpoint) -> Option<Listener> {
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&endpoint.lock_path)
        .ok()?;
    // Safety: the fd is owned by `lock` and outlives this call.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return None;
    }
    // Holding the lock means any socket still on disk is from a dead server.
    let _ = std::fs::remove_file(&endpoint.sock_path);
    let inner = UnixListener::bind(&endpoint.sock_path).ok()?;
    Some(Listener { inner, _lock: lock })
}

/// Connect to whichever process won the election.
pub(crate) fn connect(endpoint: &Endpoint) -> Option<Conn> {
    UnixStream::connect(&endpoint.sock_path).ok().map(Conn)
}

/// Unblock a thread sitting in [`Listener::accept`].
pub(crate) fn wake_listener(endpoint: &Endpoint) {
    let _ = UnixStream::connect(&endpoint.sock_path);
}

/// Release the name after serving, so the next process can bind it.
pub(crate) fn release(endpoint: &Endpoint) {
    let _ = std::fs::remove_file(&endpoint.sock_path);
}
