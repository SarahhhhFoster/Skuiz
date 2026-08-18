//! Per-platform cross-process transport.
//!
//! Both backends expose the same handful of items, so the bus above this
//! layer — the registry, in-process delivery, election bookkeeping and the
//! length-prefixed framing — is identical everywhere and is the part that
//! carries the test suite.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;
