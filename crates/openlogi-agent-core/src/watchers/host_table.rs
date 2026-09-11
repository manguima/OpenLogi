//! Keep the keyboard's host table published.
//!
//! The table is what turns a slot number into a machine: `0x1815` says slot 1
//! is `DESKTOP-0B5NC53`, and that name is both how a peer is addressed and how
//! this host knows which slot it occupies. Reading it costs HID++ round trips,
//! so it is read when the links change rather than on every switch.

use std::sync::Arc;
use std::thread;

use openlogi_core::hid::HostTable;
use openlogi_hid::{ChannelPool, DeviceIoGate};
use tokio::sync::watch;
use tracing::{debug, warn};

use super::host_switch::HostSwitchLinks;

/// Read-only view of the keyboard's host table.
pub type HostTableView = watch::Receiver<Option<Arc<HostTable>>>;

/// Spawn the reader.
///
/// The view is the only way to observe the table, so dropping it would leave a
/// thread reading the device for nobody.
///
/// Its own thread and runtime, like the other HID++ watchers: the reads are
/// slow and blocking, and a display server or a sleeping device must not stall
/// anything else.
#[must_use]
pub fn spawn(
    links: &HostSwitchLinks,
    channel_pool: ChannelPool,
    device_io: DeviceIoGate,
) -> HostTableView {
    let mut links = links.clone();
    let (table_tx, table) = watch::channel(None);
    thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                warn!(%error, "host table watcher: could not build tokio runtime");
                return;
            }
        };
        runtime.block_on(read_on_change(
            &mut links,
            &channel_pool,
            &mut device_io.clone(),
            &table_tx,
        ));
    });
    table
}

async fn read_on_change(
    links: &mut HostSwitchLinks,
    channel_pool: &ChannelPool,
    device_io: &mut DeviceIoGate,
    table_tx: &watch::Sender<Option<Arc<HostTable>>>,
) {
    loop {
        let published = Arc::clone(&links.borrow_and_update());
        // A table read is device I/O, so it waits for the same gate every
        // other proactive read does rather than poking a sleeping bus.
        if device_io.allows_io()
            && let Some(link) = published.first()
        {
            match openlogi_hid::session::host_switch::read_host_table(&link.keyboard, channel_pool)
                .await
            {
                Ok(Some(table)) => {
                    debug!(
                        current = table.current,
                        slots = table.slots.len(),
                        "host table read"
                    );
                    let _ = table_tx.send(Some(Arc::new(table)));
                }
                // A device without 0x1815 is a normal single-host device, not
                // a failure; publishing None keeps callers from waiting on a
                // table that will never arrive.
                Ok(None) => {
                    let _ = table_tx.send(None);
                }
                Err(error) => {
                    debug!(%error, "host table unreadable");
                }
            }
        }
        if links.changed().await.is_err() {
            return;
        }
    }
}
