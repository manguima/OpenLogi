//! Pointer reads and warps over X11.
//!
//! The root window's geometry is the virtual screen — the union of every
//! monitor — so its edges are the desktop's outer edges.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

use super::{Bounds, PointerError, Sample};

/// An open X11 connection and the screen's root window.
pub(super) struct Pointer {
    connection: RustConnection,
    root: Window,
}

impl Pointer {
    /// Open the display named by the environment.
    pub(super) fn connect() -> Result<Self, PointerError> {
        let (connection, screen_index) =
            x11rb::connect(None).map_err(|error| PointerError::Unavailable(error.to_string()))?;
        let root = connection
            .setup()
            .roots
            .get(screen_index)
            .ok_or_else(|| PointerError::Unavailable("screen index out of range".into()))?
            .root;
        Ok(Self { connection, root })
    }

    /// Read the pointer and the desktop bounds in one tick.
    pub(super) fn sample(&mut self) -> Result<Sample, PointerError> {
        let pointer = self
            .connection
            .query_pointer(self.root)
            .map_err(failed)?
            .reply()
            .map_err(failed)?;
        let geometry = self
            .connection
            .get_geometry(self.root)
            .map_err(failed)?
            .reply()
            .map_err(failed)?;
        Ok(Sample {
            x: i32::from(pointer.root_x),
            y: i32::from(pointer.root_y),
            bounds: Bounds {
                min_x: 0,
                min_y: 0,
                max_x: i32::from(geometry.width).saturating_sub(1),
                max_y: i32::from(geometry.height).saturating_sub(1),
            },
        })
    }

    /// Move the pointer to an absolute root-window position.
    pub(super) fn warp(&mut self, x: i32, y: i32) -> Result<(), PointerError> {
        self.connection
            .warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, narrow(x), narrow(y))
            .map_err(failed)?;
        self.connection.flush().map_err(failed)?;
        Ok(())
    }
}

/// X11 carries pointer coordinates as `i16`; saturate rather than wrap.
fn narrow(value: i32) -> i16 {
    i16::try_from(value).unwrap_or_else(|_| {
        if value.is_negative() {
            i16::MIN
        } else {
            i16::MAX
        }
    })
}

fn failed(error: impl std::fmt::Display) -> PointerError {
    PointerError::Request(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::narrow;

    #[test]
    fn out_of_range_coordinates_saturate() {
        assert_eq!(narrow(0), 0);
        assert_eq!(narrow(1_919), 1_919);
        assert_eq!(narrow(i32::MAX), i16::MAX);
        assert_eq!(narrow(i32::MIN), i16::MIN);
    }
}
