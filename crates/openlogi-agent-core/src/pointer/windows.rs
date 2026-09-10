//! Pointer reads and warps over Win32.
//!
//! The virtual-screen metrics span every monitor and can start at a negative
//! origin, so bounds are computed from the reported origin rather than zero.

use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SetCursorPos,
};

use super::{Bounds, PointerError, Sample};

/// Win32 needs no handle: the cursor APIs are session-global.
pub(super) struct Pointer;

impl Pointer {
    /// Always succeeds — there is no connection to establish.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the facade's shape; X11 and Quartz can genuinely fail here"
    )]
    pub(super) fn connect() -> Result<Self, PointerError> {
        Ok(Self)
    }

    /// Read the cursor and the virtual-screen bounds in one tick.
    #[expect(
        clippy::unused_self,
        reason = "the facade's shape; other backends carry a live connection"
    )]
    pub(super) fn sample(&mut self) -> Result<Sample, PointerError> {
        let mut point = POINT { x: 0, y: 0 };
        #[expect(
            unsafe_code,
            reason = "GetCursorPos is the only cursor read Win32 offers"
        )]
        // SAFETY: `point` is a live, correctly typed local for the duration of
        // the call, which is all GetCursorPos requires of it.
        let read = unsafe { GetCursorPos(&raw mut point) };
        if read == 0 {
            return Err(PointerError::Request("GetCursorPos failed".into()));
        }

        let min_x = metric(SM_XVIRTUALSCREEN);
        let min_y = metric(SM_YVIRTUALSCREEN);
        let width = metric(SM_CXVIRTUALSCREEN);
        let height = metric(SM_CYVIRTUALSCREEN);
        if width <= 0 || height <= 0 {
            return Err(PointerError::Request("virtual screen has no extent".into()));
        }
        Ok(Sample {
            x: point.x,
            y: point.y,
            bounds: Bounds {
                min_x,
                min_y,
                max_x: min_x.saturating_add(width).saturating_sub(1),
                max_y: min_y.saturating_add(height).saturating_sub(1),
            },
        })
    }

    /// Move the cursor to an absolute virtual-screen position.
    #[expect(
        clippy::unused_self,
        reason = "the facade's shape; other backends carry a live connection"
    )]
    pub(super) fn warp(&mut self, x: i32, y: i32) -> Result<(), PointerError> {
        #[expect(
            unsafe_code,
            reason = "SetCursorPos is the only cursor warp Win32 offers"
        )]
        // SAFETY: SetCursorPos takes two plain integers and touches no memory
        // owned by this process.
        let moved = unsafe { SetCursorPos(x, y) };
        if moved == 0 {
            return Err(PointerError::Request("SetCursorPos failed".into()));
        }
        Ok(())
    }
}

fn metric(index: i32) -> i32 {
    #[expect(
        unsafe_code,
        reason = "virtual-screen extent is only exposed through Win32"
    )]
    // SAFETY: GetSystemMetrics reads a global and returns a plain integer; an
    // unknown index yields zero rather than misbehaving.
    unsafe {
        GetSystemMetrics(index)
    }
}
