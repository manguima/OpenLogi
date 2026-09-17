//! Reading and moving this host's pointer.
//!
//! Two callers share this: the edge watcher, which reads the pointer to notice
//! it leaving, and the peer listener, which moves it to where a peer says the
//! pointer arrived. Both need the same X11 and Win32 backends, so they live
//! here rather than inside either.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "windows")]
use windows as platform;

/// Inclusive bounding box of the desktop, in the OS's virtual-screen space.
///
/// The union of every monitor, so the edges tested are the outer edges of the
/// whole desktop rather than of whichever monitor the pointer is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Leftmost x.
    pub min_x: i32,
    /// Topmost y.
    pub min_y: i32,
    /// Rightmost x.
    pub max_x: i32,
    /// Bottommost y.
    pub max_y: i32,
}

/// One pointer observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Pointer x, in virtual-screen space.
    pub x: i32,
    /// Pointer y, in virtual-screen space.
    pub y: i32,
    /// The desktop the pointer is on.
    pub bounds: Bounds,
}

/// Why the pointer could not be read or moved.
#[derive(Debug, thiserror::Error)]
pub enum PointerError {
    /// No usable display server connection on this host.
    // Win32's cursor APIs are session-global, so that backend has no connect
    // step that can fail and never builds this variant.
    #[cfg_attr(
        target_os = "windows",
        expect(clippy::allow_attributes, reason = "see above"),
        allow(dead_code, reason = "constructed only by the X11 and macOS backends")
    )]
    #[error("no display server available: {0}")]
    Unavailable(String),
    /// The connection was established but a request failed.
    // The macOS backend is a stub whose `connect` never succeeds, so `sample`
    // and `warp` — the only operations that can fail a request — are
    // unreachable there and that backend never builds this variant.
    #[cfg_attr(
        target_os = "macos",
        expect(clippy::allow_attributes, reason = "see above"),
        allow(dead_code, reason = "constructed only by the X11 and Win32 backends")
    )]
    #[error("pointer request failed: {0}")]
    Request(String),
}

/// This host's pointer.
pub struct Pointer(platform::Pointer);

impl Pointer {
    /// Open whatever the platform needs to read and move the pointer.
    pub fn connect() -> Result<Self, PointerError> {
        platform::Pointer::connect().map(Self)
    }

    /// Read the pointer and the desktop bounds in one tick.
    pub fn sample(&mut self) -> Result<Sample, PointerError> {
        self.0.sample()
    }

    /// Move the pointer to an absolute virtual-screen position.
    pub fn warp(&mut self, x: i32, y: i32) -> Result<(), PointerError> {
        self.0.warp(x, y)
    }
}
