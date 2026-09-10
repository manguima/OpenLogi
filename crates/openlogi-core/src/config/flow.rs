//! Pointer-driven host switching: which host lies past each screen edge.
//!
//! Easy-Switch moves the devices when a *key* is pressed. Flow moves them when
//! the pointer leaves the desktop, so the host change follows the direction the
//! user was already heading.

use nutype::nutype;
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

/// Lowest Easy-Switch channel a device can carry. The numbers printed on the
/// devices start at 1, so zero names no channel at all.
const FIRST_EASY_SWITCH_CHANNEL: u8 = 1;

/// A 1-based Easy-Switch channel, as printed on the device.
///
/// The HID++ layer indexes hosts from zero, so the two conventions are
/// deliberately distinct types and the only way from this one to that one is
/// [`EasySwitchChannel::host_index`]. A configured `0` is rejected when the
/// file is parsed rather than wrapping around on that conversion.
#[nutype(
    const_fn,
    validate(greater_or_equal = FIRST_EASY_SWITCH_CHANNEL),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        TryFrom,
        Into,
        Display,
        Serialize,
        Deserialize
    )
)]
pub struct EasySwitchChannel(u8);

impl EasySwitchChannel {
    /// The channel as configured, matching the number printed on the device.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.into_inner()
    }

    /// The same host, as the 0-based index the HID++ host features address it
    /// by.
    ///
    /// Cannot underflow: the wrapped value is at least 1 by construction, so a
    /// device reporting `host_count` hosts accepts exactly the channels
    /// `1..=host_count`.
    #[must_use]
    pub const fn host_index(self) -> u8 {
        self.into_inner() - FIRST_EASY_SWITCH_CHANNEL
    }
}

/// The host each edge leads to.
///
/// Values are [`EasySwitchChannel`]s — the numbers printed on the devices. An
/// edge left unset never triggers a switch, which is what makes a two-machine
/// left/right setup expressible without naming the other two edges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FlowEdges {
    /// Channel reached by leaving through the left edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<EasySwitchChannel>,
    /// Channel reached by leaving through the right edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right: Option<EasySwitchChannel>,
    /// Channel reached by leaving through the top edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top: Option<EasySwitchChannel>,
    /// Channel reached by leaving through the bottom edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bottom: Option<EasySwitchChannel>,
}

impl FlowEdges {
    /// The channel configured for `edge`, if any.
    #[must_use]
    pub fn host_for(self, edge: Edge) -> Option<EasySwitchChannel> {
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
    use super::{EasySwitchChannel, Edge, FlowConfig, FlowEdges};

    fn channel(number: u8) -> EasySwitchChannel {
        EasySwitchChannel::try_new(number).expect("test channels are non-zero")
    }

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
            left: Some(channel(1)),
            right: Some(channel(3)),
            ..FlowEdges::default()
        };
        assert_eq!(edges.host_for(Edge::Left), Some(channel(1)));
        assert_eq!(edges.host_for(Edge::Right), Some(channel(3)));
        assert_eq!(edges.host_for(Edge::Top), None);
        assert!(!edges.is_empty());
    }

    #[test]
    fn configured_channels_are_one_based_device_indices_are_not() {
        // The number printed on the device is one ahead of the index HID++
        // addresses that host by: channel 1 is the device's host 0.
        assert_eq!(channel(1).host_index(), 0);
        assert_eq!(channel(2).host_index(), 1);
        assert_eq!(channel(3).host_index(), 2);
        // A three-host device accepts 1..=3, so the top of that range must land
        // inside the `0..host_count` bound the device layer checks.
        assert!(channel(3).host_index() < 3);
        assert_eq!(channel(u8::MAX).host_index(), u8::MAX - 1);
        assert_eq!(channel(2).get(), 2);
    }

    #[test]
    fn zero_names_no_channel() {
        assert!(
            EasySwitchChannel::try_new(0).is_err(),
            "0 is not an Easy-Switch channel and must not wrap to host 255",
        );
    }

    #[test]
    fn a_zero_channel_is_rejected_when_the_file_is_parsed() {
        let rejected = toml::from_str::<FlowEdges>("right = 0");
        assert!(
            rejected.is_err(),
            "0 is not an Easy-Switch channel and must not wrap to host 255",
        );
    }

    #[test]
    fn channels_serialize_as_the_printed_number() -> Result<(), Box<dyn std::error::Error>> {
        let edges = FlowEdges {
            right: Some(channel(2)),
            ..FlowEdges::default()
        };
        let written = toml::to_string(&edges)?;
        assert_eq!(written.trim(), "right = 2");
        assert_eq!(toml::from_str::<FlowEdges>(&written)?, edges);
        Ok(())
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
        assert_eq!(parsed.edges.right, Some(channel(3)));
        // Untouched fields keep their defaults rather than zeroing out.
        assert_eq!(parsed.cooldown_ms, FlowConfig::default().cooldown_ms);
        Ok(())
    }
}
