//! Pointer-driven host switching: which host lies past each screen edge.
//!
//! Easy-Switch moves the devices when a *key* is pressed. Flow moves them when
//! the pointer leaves the desktop, so the host change follows the direction the
//! user was already heading.

use serde::{Deserialize, Serialize};

/// One edge of the desktop's bounding box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Edge {
    /// Minimum x.
    Left,
    /// Maximum x.
    Right,
    /// Minimum y.
    Top,
    /// Maximum y.
    Bottom,
}

impl Edge {
    /// Every edge, in the order the watcher tests them.
    pub const ALL: [Self; 4] = [Self::Left, Self::Right, Self::Top, Self::Bottom];
}

/// The host each edge leads to.
///
/// Values are 1-based Easy-Switch channels, matching the numbers printed on the
/// devices. An edge left unset never triggers a switch, which is what makes a
/// two-machine left/right setup expressible without naming the other two edges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FlowEdges {
    /// Host reached by leaving through the left edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<u8>,
    /// Host reached by leaving through the right edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right: Option<u8>,
    /// Host reached by leaving through the top edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top: Option<u8>,
    /// Host reached by leaving through the bottom edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bottom: Option<u8>,
}

impl FlowEdges {
    /// The host configured for `edge`, if any.
    #[must_use]
    pub fn host_for(self, edge: Edge) -> Option<u8> {
        match edge {
            Edge::Left => self.left,
            Edge::Right => self.right,
            Edge::Top => self.top,
            Edge::Bottom => self.bottom,
        }
    }

    /// Whether no edge leads anywhere.
    ///
    /// Takes `&self` because serde's `skip_serializing_if` calls it by
    /// reference.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Pointer-driven host switching.
///
/// Disabled by default: an edge that silently moves the keyboard to another
/// machine is hostile to a user who never asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FlowConfig {
    /// Master switch for the edge watcher.
    pub enabled: bool,
    /// Which host each edge leads to.
    #[serde(skip_serializing_if = "FlowEdges::is_empty")]
    pub edges: FlowEdges,
    /// How long the pointer must hold against an edge before it counts.
    /// Guards against overshooting a maximize button on the far monitor.
    pub dwell_ms: u16,
    /// How far to pull the pointer back after switching, so the next frame does
    /// not read as a fresh edge contact.
    pub rebound_px: u16,
    /// Dead time after a switch. Covers the device reconnect, which is hundreds
    /// of milliseconds on Bluetooth and during which a second trigger is noise.
    pub cooldown_ms: u16,
    /// Pointer sampling rate.
    pub poll_hz: u16,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            edges: FlowEdges::default(),
            dwell_ms: 120,
            rebound_px: 8,
            cooldown_ms: 1500,
            poll_hz: 60,
        }
    }
}

impl FlowConfig {
    /// Whether every field still holds its default, so the section can be left
    /// out of the written file.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Whether the watcher has anything to do.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled && !self.edges.is_empty()
    }

    /// Sampling interval, clamped so a zero or absurd `poll_hz` cannot spin the
    /// watcher thread.
    #[must_use]
    pub fn poll_interval(&self) -> std::time::Duration {
        let hz = self.poll_hz.clamp(1, 240);
        std::time::Duration::from_micros(1_000_000 / u64::from(hz))
    }
}

#[cfg(test)]
mod tests {
    use super::{Edge, FlowConfig, FlowEdges};

    #[test]
    fn default_section_is_omitted_and_inert() {
        let config = FlowConfig::default();
        assert!(config.is_default());
        assert!(!config.is_active());
        assert!(config.edges.is_empty());
    }

    #[test]
    fn enabled_without_edges_stays_inert() {
        let config = FlowConfig {
            enabled: true,
            ..FlowConfig::default()
        };
        assert!(!config.is_active());
    }

    #[test]
    fn edges_map_to_hosts() {
        let edges = FlowEdges {
            left: Some(1),
            right: Some(3),
            ..FlowEdges::default()
        };
        assert_eq!(edges.host_for(Edge::Left), Some(1));
        assert_eq!(edges.host_for(Edge::Right), Some(3));
        assert_eq!(edges.host_for(Edge::Top), None);
        assert!(!edges.is_empty());
    }

    #[test]
    fn poll_interval_survives_nonsense_rates() {
        let slowest = FlowConfig {
            poll_hz: 0,
            ..FlowConfig::default()
        };
        assert_eq!(slowest.poll_interval().as_micros(), 1_000_000);
        let fastest = FlowConfig {
            poll_hz: u16::MAX,
            ..FlowConfig::default()
        };
        assert!(fastest.poll_interval().as_micros() >= 4_166);
    }

    #[test]
    fn round_trips_through_toml() -> Result<(), Box<dyn std::error::Error>> {
        let parsed: FlowConfig = toml::from_str(
            "
            enabled = true
            dwell_ms = 200

            [edges]
            left = 1
            right = 3
            ",
        )?;
        assert!(parsed.is_active());
        assert_eq!(parsed.dwell_ms, 200);
        assert_eq!(parsed.edges.right, Some(3));
        // Untouched fields keep their defaults rather than zeroing out.
        assert_eq!(parsed.cooldown_ms, FlowConfig::default().cooldown_ms);
        Ok(())
    }
}
