//! Pointer backend placeholder for macOS.
//!
//! Quartz exposes both halves (`CGEventGetLocation`, `CGWarpMouseCursorPosition`),
//! but wiring them means pulling the Core Graphics FFI into this crate under
//! the contract in `.claude/rules/objc-ffi.md`. Until that lands, the watcher
//! reports the backend as unavailable and stays inert instead of pretending the
//! edges work.

use std::convert::Infallible;

use super::{PointerError, Sample};

/// Uninhabited: [`Pointer::connect`] never returns one.
pub(super) struct Pointer(Infallible);

impl Pointer {
    /// Always fails, which parks the watcher until the section changes.
    pub(super) fn connect() -> Result<Self, PointerError> {
        Err(PointerError::Unavailable(
            "pointer-edge switching is not implemented on macOS yet".into(),
        ))
    }

    /// Unreachable by construction.
    pub(super) fn sample(&mut self) -> Result<Sample, PointerError> {
        match self.0 {}
    }

    /// Unreachable by construction.
    pub(super) fn warp(&mut self, _x: i32, _y: i32) -> Result<(), PointerError> {
        match self.0 {}
    }
}
