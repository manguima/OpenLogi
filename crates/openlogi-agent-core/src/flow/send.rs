//! Handing the pointer to a peer.
//!
//! The whole value of this is ordering: the frame goes out *before* the HID++
//! write that moves the devices. A LAN hop is about a millisecond and the
//! radio reconnect is hundreds, so the pointer is already sitting on the other
//! screen by the time the devices arrive. Reverse the two and the peer would
//! land the pointer after the user had already noticed the gap.

use std::time::Duration;

use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::debug;

use super::Handoff;
use super::seal::{Seal, unix_now};
use super::wire::encode;

/// How long a peer has to accept the handoff before it is abandoned.
///
/// Short on purpose. A peer that is asleep, gone, or on another network must
/// not hold up the switch — the devices move either way, and a late handoff is
/// worth nothing once the user has already seen the gap it was hiding.
const SEND_TIMEOUT: Duration = Duration::from_millis(400);

/// Send `handoff` to the first candidate address that accepts it.
///
/// Candidates are tried in order because a device stores a bare host name and
/// only mDNS answers for it, so the same peer is reachable under more than one
/// spelling. The first success wins; there is nothing to retry, since the
/// switch has already happened by the time a retry could land.
pub async fn deliver(handoff: &Handoff, seal: &Seal, nonce: u64, candidates: &[String]) {
    let frame = encode(&seal.seal(handoff, unix_now(), nonce));
    for address in candidates {
        match timeout(SEND_TIMEOUT, write_to(address, &frame)).await {
            Ok(Ok(())) => {
                debug!(address, host = handoff.host, "flow: handed off to peer");
                return;
            }
            Ok(Err(error)) => debug!(address, %error, "flow: peer unreachable"),
            Err(_) => debug!(address, "flow: peer timed out"),
        }
    }
    debug!(
        host = handoff.host,
        candidates = candidates.len(),
        "flow: no peer took the handoff — switching without cover"
    );
}

async fn write_to(address: &str, frame: &[u8]) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(address).await?;
    // Nagle would sit on a 61-byte write waiting for more; there is no more.
    stream.set_nodelay(true)?;
    stream.write_all(frame).await?;
    stream.flush().await
}

/// A nonce for one handoff.
///
/// Uniqueness within the freshness window is all that is required — the nonce
/// stops a captured frame being replayed, and the tag is what stops forgery,
/// so this does not have to be unpredictable. Nanoseconds since the epoch are
/// unique at any rate a pointer can cross an edge.
#[must_use]
pub fn nonce() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::nonce;

    #[test]
    fn nonces_differ_between_calls() {
        // Two handoffs in the same second must not share a nonce, or the
        // second would be refused as a replay of the first.
        let first = nonce();
        let second = nonce();
        assert_ne!(first, second);
    }
}
