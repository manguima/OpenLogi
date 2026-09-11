//! Read the host table a device exposes through `HostsInfo` (`0x1815`).
//!
//! `ChangeHost` can move a device between slots but says nothing about them.
//! This is what turns "host 0" into "DESKTOP-0B5NC53" for anything that has to
//! show the table to a person rather than just address it.

use hidpp::{
    device::Device,
    feature::{
        CreatableFeature,
        hosts_info::{HostIndex, HostSlotStatus, HostsInfoFeature},
    },
};
use tracing::debug;

use super::{HostSwitchError, open_channel, timed_hidpp};
use openlogi_core::hid::{HostSlot, HostTable};

use crate::{ChannelPool, DeviceRoute};

/// Read `route`'s host table.
///
/// `Ok(None)` means the device does not implement `HostsInfo` — a normal answer
/// for a single-host device, not a failure. Per-slot reads are advisory: a slot
/// whose descriptor cannot be read still appears, just without a name, because
/// a missing label is no reason to hide a slot the user can switch to.
pub async fn read_host_table(
    route: &DeviceRoute,
    channel_pool: &ChannelPool,
) -> Result<Option<HostTable>, HostSwitchError> {
    let Some(channel) = open_channel(channel_pool, route, "opening host table channel").await?
    else {
        return Ok(None);
    };
    let device_index = route.device_index();
    let mut device = timed_hidpp(
        "opening host table device",
        Device::new(channel, device_index),
    )
    .await?;
    let Some(feature) = timed_hidpp(
        "locating hosts-info feature",
        device.root().get_feature(HostsInfoFeature::ID),
    )
    .await?
    else {
        return Ok(None);
    };
    let hosts_info = device.add_feature::<HostsInfoFeature>(feature.index);

    let info = timed_hidpp("reading host table size", hosts_info.get_feature_info()).await?;
    // `HostIndex` is non-exhaustive and its non-slot forms name "wherever the
    // device is" rather than an index. Falling back to the first slot keeps a
    // table renderable; nothing here writes a host, so a wrong guess only
    // mislabels which entry is highlighted.
    let current = match info.current_host {
        HostIndex::Slot(slot) => slot,
        _ => 0,
    };

    let mut slots = Vec::with_capacity(usize::from(info.host_count));
    for index in 0..info.host_count {
        slots.push(read_slot(&hosts_info, index).await);
    }
    Ok(Some(HostTable { current, slots }))
}

/// One slot, degrading to "present but unnamed" on any read failure.
async fn read_slot(hosts_info: &HostsInfoFeature, index: u8) -> HostSlot {
    let info = match timed_hidpp(
        "reading host slot",
        hosts_info.get_host_info(HostIndex::Slot(index)),
    )
    .await
    {
        Ok(info) => info,
        Err(error) => {
            debug!(index, %error, "host slot unreadable; reporting it unnamed");
            return HostSlot {
                index,
                paired: false,
                name: None,
            };
        }
    };
    let paired = info.status != HostSlotStatus::Empty;
    let name = read_name(hosts_info, index, info.name_len).await;
    HostSlot {
        index,
        paired,
        name,
    }
}

/// Read a slot's friendly name, one chunk at a time.
///
/// The device reports the length separately from the chunks, so the tail is
/// truncated to it rather than trimmed of what looks like padding — a name may
/// legitimately end in a byte that resembles filler.
async fn read_name(hosts_info: &HostsInfoFeature, index: u8, name_len: u8) -> Option<String> {
    if name_len == 0 {
        return None;
    }
    let wanted = usize::from(name_len);
    let mut raw: Vec<u8> = Vec::with_capacity(wanted);
    while raw.len() < wanted {
        let offset = u8::try_from(raw.len()).ok()?;
        let chunk = match timed_hidpp(
            "reading host name",
            hosts_info.get_host_friendly_name(HostIndex::Slot(index), offset),
        )
        .await
        {
            Ok(chunk) => chunk,
            Err(error) => {
                debug!(index, offset, %error, "host name chunk unreadable; using what was read");
                break;
            }
        };
        // A device that ignores the offset would loop forever handing back the
        // same chunk; trust its own answer about where this one starts.
        if usize::from(chunk.byte_index) != raw.len() {
            debug!(
                index,
                asked = offset,
                got = chunk.byte_index,
                "host name chunk came back at the wrong offset"
            );
            break;
        }
        raw.extend_from_slice(&chunk.chunk);
    }
    raw.truncate(wanted);
    decode_name(&raw)
}

/// Decode a descriptor body to a display name.
///
/// The field is a fixed-width byte array, so trailing NULs are padding rather
/// than content, and a device that never had a name written reports blanks.
fn decode_name(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let trimmed = text.trim_end_matches('\0').trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::decode_name;

    #[test]
    fn padding_and_blanks_are_not_names() {
        assert_eq!(decode_name(b""), None);
        assert_eq!(decode_name(&[0; 14]), None);
        assert_eq!(decode_name(b"   "), None);
    }

    #[test]
    fn a_padded_name_keeps_its_content() {
        let mut raw = b"DESKTOP-0B5NC53".to_vec();
        raw.extend_from_slice(&[0; 13]);
        assert_eq!(decode_name(&raw).as_deref(), Some("DESKTOP-0B5NC53"));
    }

    #[test]
    fn invalid_utf8_degrades_instead_of_failing() {
        // A slot written by another OS can hold bytes this host cannot decode;
        // a replacement char is more useful than hiding the slot.
        let decoded = decode_name(&[b'A', 0xff, b'B']).expect("lossy decode keeps the ASCII");
        assert!(decoded.starts_with('A'));
        assert!(decoded.ends_with('B'));
    }
}
