//! Pointer-driven host switching: which host lies past each screen edge.
//!
//! Easy-Switch moves the devices when a *key* is pressed. Flow moves them when
//! the pointer leaves the desktop, so the host change follows the direction the
//! user was already heading.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One edge of the desktop's bounding box.
///
/// Serializable because a peer handoff names the edge the pointer left
/// through; the variant order is therefore part of that wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

    /// The byte that names this edge on the wire.
    ///
    /// Spelled out rather than derived from the declaration order, so
    /// reordering the variants cannot silently change what a peer receives.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Right => 1,
            Self::Top => 2,
            Self::Bottom => 3,
        }
    }

    /// The edge a wire byte names, if it names one.
    #[must_use]
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Left),
            1 => Some(Self::Right),
            2 => Some(Self::Top),
            3 => Some(Self::Bottom),
            _ => None,
        }
    }
}

/// A host as the user names it: the 1-based Easy-Switch channel printed on
/// the device.
///
/// HID++ indexes hosts from zero. Keeping the two apart in the type system is
/// what stops a config value reaching `CHANGE_HOST` unconverted — a mistake
/// that reads as "nothing happened", because channel 1 lands on index 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostChannel(u8);

impl HostChannel {
    /// Wrap a 1-based channel, rejecting the meaningless zero.
    #[must_use]
    pub fn new(channel: u8) -> Option<Self> {
        (channel >= 1).then_some(Self(channel))
    }

    /// The 1-based channel, as printed on the device.
    #[must_use]
    pub fn channel(self) -> u8 {
        self.0
    }

    /// The 0-based index HID++ `CHANGE_HOST` expects.
    #[must_use]
    pub fn index(self) -> u8 {
        self.0 - 1
    }
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
    ///
    /// A zero in the file is treated as unset rather than as index 0: the
    /// numbers here are the ones printed on the device, which start at one.
    #[must_use]
    pub fn host_for(self, edge: Edge) -> Option<HostChannel> {
        let channel = match edge {
            Edge::Left => self.left,
            Edge::Right => self.right,
            Edge::Top => self.top,
            Edge::Bottom => self.bottom,
        };
        channel.and_then(HostChannel::new)
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

/// The peer link that covers a switch's radio reconnect.
///
/// Off unless a secret is set. Without a peer the switch still happens; the
/// pointer simply does not appear on the arriving host until its own OS moves
/// it, which is the difference between "the cursor jumped over" and "the
/// cursor vanished for half a second".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PeerConfig {
    /// Shared secret every peer must carry. Absent means no peer link: a
    /// handoff service that anyone on the network could drive is worse than
    /// no handoff at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// Port peers listen on. Zero means the built-in default.
    pub port: u16,
    /// Addresses for hosts whose name the network will not resolve, keyed by
    /// the name the device stores for that slot.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub addresses: BTreeMap<String, String>,
}

impl PeerConfig {
    /// The port to use, resolving zero to the default.
    #[must_use]
    pub fn port(&self) -> u16 {
        if self.port == 0 {
            DEFAULT_PEER_PORT
        } else {
            self.port
        }
    }

    /// Whether a peer link is configured at all.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.secret
            .as_ref()
            .is_some_and(|secret| !secret.is_empty())
    }

    /// Whether every field still holds its default.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Where to reach the host the device calls `name`.
    ///
    /// An explicit override wins; otherwise the name itself is the address,
    /// which is what makes the common case need no configuration — the device
    /// already stores what the other machine calls itself.
    #[must_use]
    pub fn address_for(&self, name: &str) -> String {
        let host = self.addresses.get(name).map_or(name, String::as_str);
        format!("{host}:{}", self.port())
    }
}

/// Port peers listen on when the config does not say.
///
/// Deliberately not Logitech Flow's 59867: a machine running both should not
/// have them collide.
pub const DEFAULT_PEER_PORT: u16 = 59870;

/// Pointer-driven host switching.
///
/// Disabled by default: an edge that silently moves the keyboard to another
/// machine is hostile to a user who never asked for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The peer link that makes a switch look instant.
    #[serde(skip_serializing_if = "PeerConfig::is_default")]
    pub peers: PeerConfig,
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
            peers: PeerConfig::default(),
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
    use super::{DEFAULT_PEER_PORT, Edge, FlowConfig, FlowEdges, HostChannel, PeerConfig};

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
    fn every_edge_round_trips_through_its_wire_byte() {
        for edge in Edge::ALL {
            assert_eq!(Edge::from_code(edge.code()), Some(edge));
        }
        assert_eq!(Edge::from_code(4), None);
        assert_eq!(Edge::from_code(u8::MAX), None);
    }

    #[test]
    fn edges_map_to_hosts() {
        let edges = FlowEdges {
            left: Some(1),
            right: Some(3),
            ..FlowEdges::default()
        };
        assert_eq!(
            edges.host_for(Edge::Left).map(HostChannel::channel),
            Some(1)
        );
        assert_eq!(
            edges.host_for(Edge::Right).map(HostChannel::channel),
            Some(3)
        );
        assert_eq!(edges.host_for(Edge::Top), None);
        assert!(!edges.is_empty());
    }

    #[test]
    fn channel_one_is_hidpp_index_zero() {
        // The bug this type exists to prevent: channel 1 is the *first* host,
        // which CHANGE_HOST addresses as 0.
        let first = HostChannel::new(1).expect("1 is a valid channel");
        assert_eq!(first.channel(), 1);
        assert_eq!(first.index(), 0);
        let third = HostChannel::new(3).expect("3 is a valid channel");
        assert_eq!(third.index(), 2);
    }

    #[test]
    fn zero_is_not_a_channel() {
        assert_eq!(HostChannel::new(0), None);
        let edges = FlowEdges {
            left: Some(0),
            ..FlowEdges::default()
        };
        assert_eq!(edges.host_for(Edge::Left), None);
    }

    #[test]
    fn a_peer_link_needs_a_non_empty_secret() {
        assert!(!PeerConfig::default().is_active());
        let blank = PeerConfig {
            secret: Some(String::new()),
            ..PeerConfig::default()
        };
        assert!(!blank.is_active(), "an empty secret is not a secret");
        let set = PeerConfig {
            secret: Some("shared".into()),
            ..PeerConfig::default()
        };
        assert!(set.is_active());
    }

    #[test]
    fn the_device_stored_name_is_the_address_by_default() {
        // The whole point: the device already knows what the other machine
        // calls itself, so the common case needs no configuration.
        let peers = PeerConfig::default();
        assert_eq!(
            peers.address_for("DESKTOP-0B5NC53"),
            format!("DESKTOP-0B5NC53:{DEFAULT_PEER_PORT}")
        );
    }

    #[test]
    fn an_override_wins_for_a_name_the_network_cannot_resolve() {
        let mut peers = PeerConfig {
            port: 40000,
            ..PeerConfig::default()
        };
        peers
            .addresses
            .insert("DESKTOP-0B5NC53".into(), "192.168.1.20".into());
        assert_eq!(peers.address_for("DESKTOP-0B5NC53"), "192.168.1.20:40000");
        assert_eq!(
            peers.address_for("LAPTOP-OM1TP89K"),
            "LAPTOP-OM1TP89K:40000"
        );
    }

    #[test]
    fn port_zero_means_the_default() {
        assert_eq!(PeerConfig::default().port(), DEFAULT_PEER_PORT);
        let explicit = PeerConfig {
            port: 1234,
            ..PeerConfig::default()
        };
        assert_eq!(explicit.port(), 1234);
    }

    #[test]
    fn a_peer_section_round_trips_and_leaves_flow_defaults_alone()
    -> Result<(), Box<dyn std::error::Error>> {
        let parsed: FlowConfig = toml::from_str(
            "
            enabled = true

            [edges]
            left = 1

            [peers]
            secret = \"s3cr3t\"

            [peers.addresses]
            \"DESKTOP-0B5NC53\" = \"192.168.1.20\"
            ",
        )?;
        assert!(parsed.peers.is_active());
        assert_eq!(parsed.peers.port(), DEFAULT_PEER_PORT);
        assert_eq!(
            parsed.peers.address_for("DESKTOP-0B5NC53"),
            "192.168.1.20:59870"
        );
        assert_eq!(parsed.dwell_ms, FlowConfig::default().dwell_ms);
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
        assert_eq!(parsed.edges.right, Some(3));
        // Untouched fields keep their defaults rather than zeroing out.
        assert_eq!(parsed.cooldown_ms, FlowConfig::default().cooldown_ms);
        Ok(())
    }
}
