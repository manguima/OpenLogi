//! Receiving handoffs from a peer and landing the pointer.
//!
//! The listener exists only while a secret is configured. An unauthenticated
//! service that moves the user's pointer would be worse than the delay it is
//! meant to hide, so "no secret" means "no socket", not "an open socket".

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use super::seal::{ReplayGuard, Seal, unix_now};
use super::wire::{FRAME_LEN, decode};
use crate::pointer::Pointer;
use crate::watchers::edge_switch::FlowSettings;

/// How long a connection may take to deliver its frame.
///
/// A handoff is 61 bytes on a LAN. Anything slower is not a peer in a hurry to
/// hand over the pointer, and holding the accept loop for it is how one stalled
/// connection would deny every real one.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// This host's own Easy-Switch slot, when it is known.
///
/// A handoff names the slot it is for. Knowing our own turns a peer's mistake
/// into a discarded message instead of a pointer that jumps on the wrong
/// machine; not knowing it is not fatal, because the sender addressed us
/// directly, so an unknown slot accepts and says so.
pub type OwnSlot = watch::Receiver<Option<u8>>;

/// Spawn the peer listener.
///
/// Owns a thread and a current-thread runtime for the same reason the edge
/// watcher does: a warp is a blocking display-server round trip, and it has no
/// business sharing a runtime with the HID++ sessions.
pub fn spawn(flow: &FlowSettings, slot: OwnSlot) {
    let mut flow = flow.clone();
    thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                warn!(%error, "flow: could not build the peer listener runtime");
                return;
            }
        };
        runtime.block_on(serve(&mut flow, &slot));
    });
}

async fn serve(flow: &mut FlowSettings, slot: &OwnSlot) {
    loop {
        let config = Arc::clone(&flow.borrow_and_update());
        let Some(secret) = config
            .peers
            .secret
            .as_ref()
            .filter(|secret| !secret.is_empty())
        else {
            // No secret means no socket. Park until the section changes.
            if flow.changed().await.is_err() {
                return;
            }
            continue;
        };
        let seal = Seal::new(secret);
        let port = config.peers.port();
        // The same distance the watcher rebounds by: one knob for "how far
        // clear of an edge the pointer has to be", used on both sides of it.
        let inset = i32::from(config.rebound_px);

        let listener = match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => {
                info!(port, "flow: listening for peer handoffs");
                listener
            }
            Err(error) => {
                warn!(%error, port, "flow: could not listen for peer handoffs");
                if flow.changed().await.is_err() {
                    return;
                }
                continue;
            }
        };

        let mut guard = ReplayGuard::default();
        loop {
            tokio::select! {
                // A config change retires the socket: the secret or the port
                // may have moved, and serving on the old one would be serving
                // something the user has already revoked.
                changed = flow.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    break;
                }
                accepted = listener.accept() => match accepted {
                    Ok((stream, from)) => {
                        debug!(%from, "flow: peer connected");
                        land(stream, &seal, &mut guard, slot, inset).await;
                    }
                    Err(error) => {
                        warn!(%error, "flow: peer accept failed");
                        break;
                    }
                },
            }
        }
    }
}

/// Read one framed handoff and move the pointer to where it says.
///
/// Handled inline rather than in a spawned task: a handoff is one tiny frame,
/// two cannot usefully arrive at once, and the read timeout is what bounds a
/// stalled peer rather than an unbounded pile of tasks.
async fn land(
    mut stream: TcpStream,
    seal: &Seal,
    guard: &mut ReplayGuard,
    slot: &OwnSlot,
    inset: i32,
) {
    let mut frame = [0; FRAME_LEN];
    match timeout(READ_TIMEOUT, stream.read_exact(&mut frame)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            debug!(%error, "flow: peer frame could not be read");
            return;
        }
        Err(_) => {
            debug!("flow: peer frame timed out");
            return;
        }
    }
    let sealed = match decode(&frame) {
        Ok(sealed) => sealed,
        Err(error) => {
            debug!(%error, "flow: peer frame rejected");
            return;
        }
    };
    let handoff = match seal.open(&sealed, unix_now(), guard) {
        Ok(handoff) => handoff,
        Err(error) => {
            // Worth a warning rather than a debug: on a correctly configured
            // pair this never happens, so it means a mismatched secret, a
            // skewed clock, or someone probing the port.
            warn!(%error, "flow: peer handoff refused");
            return;
        }
    };
    if let Some(own) = *slot.borrow()
        && own != handoff.host
    {
        debug!(
            own,
            meant_for = handoff.host,
            "flow: handoff was meant for another host"
        );
        return;
    }

    let mut pointer = match Pointer::connect() {
        Ok(pointer) => pointer,
        Err(error) => {
            warn!(%error, "flow: cannot land the pointer — no display server");
            return;
        }
    };
    let bounds = match pointer.sample() {
        Ok(sample) => sample.bounds,
        Err(error) => {
            warn!(%error, "flow: cannot land the pointer — bounds unreadable");
            return;
        }
    };
    let (x, y) = handoff.landing(
        (bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y),
        inset,
    );
    match pointer.warp(x, y) {
        Ok(()) => debug!(x, y, ?handoff.left_through, "flow: pointer landed from peer"),
        Err(error) => warn!(%error, "flow: pointer landing failed"),
    }
}
