//! Peer handoff for pointer-edge host switching.
//!
//! Switching the devices to another host costs a radio reconnect that nothing
//! in software can shorten. What a peer link buys is not speed but *cover*:
//! the arriving host places its own pointer where the pointer left, over the
//! LAN, while the radio is still catching up. The user sees the cursor land
//! immediately and has usually not moved the mouse again by the time input
//! actually arrives.
//!
//! This module is the pure half — the message and the geometry. Discovery and
//! transport live in the children.

pub mod listen;
pub mod seal;
pub mod send;
pub mod wire;

use openlogi_core::config::Edge;
use serde::{Deserialize, Serialize};

/// Where along an edge the pointer crossed, as a fraction of that edge.
///
/// A fraction rather than a pixel offset because the two desktops rarely share
/// a resolution: leaving halfway down a 1080p screen should arrive halfway
/// down a 1440p one, not 540 pixels from its top.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EdgeFraction(f32);

impl EdgeFraction {
    /// Clamp a raw fraction into range.
    ///
    /// Values outside `0..=1` are not trusted arithmetic — they arrive from a
    /// peer — so they are clamped rather than rejected: a pointer at a corner
    /// is still a pointer, and refusing the handoff would strand it.
    #[must_use]
    pub fn new(value: f32) -> Self {
        Self(if value.is_finite() {
            value.clamp(0.0, 1.0)
        } else {
            0.5
        })
    }

    /// Where `position` sits between `min` and `max`, inclusive.
    #[must_use]
    pub fn between(position: i32, min: i32, max: i32) -> Self {
        let span = max.saturating_sub(min);
        if span <= 0 {
            return Self(0.5);
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "screen spans are far below f32's exact-integer range"
        )]
        Self::new(position.saturating_sub(min) as f32 / span as f32)
    }

    /// Project the fraction back onto a span of this host's own screen.
    #[must_use]
    pub fn project(self, min: i32, max: i32) -> i32 {
        let span = max.saturating_sub(min);
        if span <= 0 {
            return min;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "screen spans are far below f32's exact-integer range"
        )]
        let offset = (self.0 * span as f32).round();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "offset is bounded by span, which came from an i32"
        )]
        let offset = offset as i32;
        min.saturating_add(offset)
    }

    /// The raw fraction.
    #[must_use]
    pub fn value(self) -> f32 {
        self.0
    }
}

/// The edge the pointer enters through on the arriving host.
///
/// Leaving through one side means entering through the opposite one — the two
/// desktops face each other, so the sender states where it left and the
/// receiver derives where that lands.
#[must_use]
pub fn opposite(edge: Edge) -> Edge {
    match edge {
        Edge::Left => Edge::Right,
        Edge::Right => Edge::Left,
        Edge::Top => Edge::Bottom,
        Edge::Bottom => Edge::Top,
    }
}

/// One host handing the pointer to another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Handoff {
    /// The 0-based host slot the devices were told to move to. The receiver
    /// checks this against its own slot before acting, so a message meant for
    /// a third machine is ignored rather than obeyed.
    pub host: u8,
    /// The edge the pointer left through, on the *sending* host.
    pub left_through: Edge,
    /// How far along that edge it crossed.
    pub at: EdgeFraction,
}

impl Handoff {
    /// Where the arriving host should place its pointer, given its own bounds.
    ///
    /// `inset` pulls the landing just inside the screen instead of onto the
    /// boundary pixel. Landing exactly on the edge is what the local edge
    /// watcher is watching for, so an arrival there reads as a departure and
    /// the two hosts bounce the pointer back and forth forever.
    #[must_use]
    pub fn landing(&self, bounds: (i32, i32, i32, i32), inset: i32) -> (i32, i32) {
        let (min_x, min_y, max_x, max_y) = bounds;
        // Never inset past the middle: a desktop narrower than twice the inset
        // would otherwise land the pointer on the far edge.
        let across = inset.clamp(0, (max_x - min_x).max(0) / 2);
        let down = inset.clamp(0, (max_y - min_y).max(0) / 2);
        match opposite(self.left_through) {
            Edge::Left => (min_x + across, self.at.project(min_y, max_y)),
            Edge::Right => (max_x - across, self.at.project(min_y, max_y)),
            Edge::Top => (self.at.project(min_x, max_x), min_y + down),
            Edge::Bottom => (self.at.project(min_x, max_x), max_y - down),
        }
    }
}

#[cfg(test)]
mod tests {
    use openlogi_core::config::Edge;

    use super::{EdgeFraction, Handoff, opposite};

    fn handoff(left_through: Edge, at: f32) -> Handoff {
        Handoff {
            host: 0,
            left_through,
            at: EdgeFraction::new(at),
        }
    }

    #[test]
    fn leaving_one_side_arrives_on_the_other() {
        assert_eq!(opposite(Edge::Left), Edge::Right);
        assert_eq!(opposite(Edge::Right), Edge::Left);
        assert_eq!(opposite(Edge::Top), Edge::Bottom);
        assert_eq!(opposite(Edge::Bottom), Edge::Top);
    }

    #[test]
    fn a_fraction_survives_a_resolution_change() {
        // Halfway down 1080p must land halfway down 1440p, not at pixel 540.
        let leaving = EdgeFraction::between(540, 0, 1079);
        assert_eq!(leaving.project(0, 1439), 720);
    }

    #[test]
    fn the_ends_of_an_edge_stay_at_the_ends() {
        let top = EdgeFraction::between(0, 0, 1079);
        let bottom = EdgeFraction::between(1079, 0, 1079);
        assert_eq!(top.project(0, 1439), 0);
        assert_eq!(bottom.project(0, 1439), 1439);
    }

    #[test]
    fn a_negative_origin_is_handled() {
        // Windows virtual screens can start left of zero.
        let middle = EdgeFraction::between(0, -1920, 1919);
        assert_eq!(middle.project(-1920, 1919), 0);
    }

    #[test]
    fn hostile_fractions_are_clamped_not_trusted() {
        // Asserted through the projection so the check stays on integers: a
        // peer-supplied float is exactly the value not to compare exactly.
        assert_eq!(EdgeFraction::new(-5.0).project(0, 100), 0);
        assert_eq!(EdgeFraction::new(9.0).project(0, 100), 100);
        assert_eq!(EdgeFraction::new(f32::NAN).project(0, 100), 50);
        assert_eq!(EdgeFraction::new(f32::INFINITY).project(0, 100), 50);
        assert_eq!(EdgeFraction::new(f32::NEG_INFINITY).project(0, 100), 50);
    }

    #[test]
    fn a_degenerate_span_does_not_divide_by_zero() {
        assert_eq!(EdgeFraction::between(10, 5, 5).project(0, 100), 50);
        assert_eq!(EdgeFraction::new(0.7).project(5, 5), 5);
    }

    #[test]
    fn leaving_right_lands_on_the_left_edge_at_the_same_height() {
        let landing = handoff(Edge::Right, 0.25).landing((0, 0, 2559, 1439), 0);
        assert_eq!(landing, (0, 360));
    }

    #[test]
    fn leaving_the_top_lands_on_the_bottom_at_the_same_width() {
        let landing = handoff(Edge::Top, 0.5).landing((0, 0, 1919, 1079), 0);
        assert_eq!(landing, (960, 1079));
    }

    #[test]
    fn an_inset_keeps_the_arrival_clear_of_the_local_edge() {
        // Measured, not theorised: landing on the boundary pixel made the
        // arriving host's own edge watcher fire and hand the pointer straight
        // back, which is a ping-pong between the two machines.
        assert_eq!(
            handoff(Edge::Right, 0.25).landing((0, 0, 2559, 1439), 12),
            (12, 360)
        );
        assert_eq!(
            handoff(Edge::Left, 0.25).landing((0, 0, 2559, 1439), 12),
            (2547, 360)
        );
        assert_eq!(
            handoff(Edge::Top, 0.5).landing((0, 0, 1919, 1079), 12),
            (960, 1067)
        );
        assert_eq!(
            handoff(Edge::Bottom, 0.5).landing((0, 0, 1919, 1079), 12),
            (960, 12)
        );
    }

    #[test]
    fn an_inset_never_overshoots_a_narrow_desktop() {
        // A silly inset must not push the pointer out the far side.
        let landing = handoff(Edge::Right, 0.5).landing((0, 0, 20, 20), 500);
        assert_eq!(landing, (10, 10));
    }

    #[test]
    fn a_negative_inset_is_treated_as_none() {
        let landing = handoff(Edge::Right, 0.25).landing((0, 0, 2559, 1439), -50);
        assert_eq!(landing, (0, 360));
    }
}
