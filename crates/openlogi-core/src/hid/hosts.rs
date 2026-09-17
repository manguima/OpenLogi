//! The host table a multi-host device exposes.
//!
//! Easy-Switch addresses hosts by a 0-based slot index while the keys printed
//! on the device count from one. Both numbers appear here so a caller never has
//! to guess which convention a field follows.

use serde::{Deserialize, Serialize};

/// One host slot as the device reports it.
///
/// Rides the IPC wire: field order is part of the format — see
/// `crates/openlogi-ipc/AGENTS.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSlot {
    /// 0-based slot index, the one `CHANGE_HOST` addresses.
    pub index: u8,
    /// Whether the slot holds a pairing. An empty slot is still listed: the
    /// user can pair it later, and hiding it would renumber the ones after it.
    pub paired: bool,
    /// Friendly name the host wrote into the device, when it stored one.
    pub name: Option<String>,
}

impl HostSlot {
    /// The 1-based Easy-Switch channel printed on the device.
    #[must_use]
    pub fn channel(&self) -> u8 {
        self.index.saturating_add(1)
    }
}

/// A device's host table and the slot it is currently on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostTable {
    /// 0-based slot the device is talking to right now.
    pub current: u8,
    /// Every slot the device exposes, in index order.
    pub slots: Vec<HostSlot>,
}

impl HostTable {
    /// The slot the device is currently on, when the table lists it.
    #[must_use]
    pub fn current_slot(&self) -> Option<&HostSlot> {
        self.slots.iter().find(|slot| slot.index == self.current)
    }
}

#[cfg(test)]
mod tests {
    use super::{HostSlot, HostTable};

    fn slot(index: u8, name: Option<&str>) -> HostSlot {
        HostSlot {
            index,
            paired: name.is_some(),
            name: name.map(str::to_string),
        }
    }

    #[test]
    fn channel_is_one_past_the_index() {
        assert_eq!(slot(0, None).channel(), 1);
        assert_eq!(slot(2, None).channel(), 3);
    }

    #[test]
    fn current_slot_is_resolved_by_index_not_position() {
        // A device that reports slots out of order must not be read
        // positionally — `current` names an index, not an offset.
        let table = HostTable {
            current: 1,
            slots: vec![slot(2, Some("laptop")), slot(1, Some("desktop"))],
        };
        assert_eq!(
            table.current_slot().and_then(|slot| slot.name.as_deref()),
            Some("desktop")
        );
    }

    #[test]
    fn a_current_slot_missing_from_the_table_is_none() {
        let table = HostTable {
            current: 5,
            slots: vec![slot(0, None)],
        };
        assert_eq!(table.current_slot(), None);
    }
}
