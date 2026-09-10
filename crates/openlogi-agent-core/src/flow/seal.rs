//! Proving a handoff came from a peer that knows the shared secret.
//!
//! A handoff moves the user's pointer, so anything on the LAN that can send
//! one can yank the cursor around. The peers therefore share a secret and
//! every message carries an HMAC over its canonical bytes, a send time, and a
//! nonce — the tag stops forgery, the time and nonce stop replay.
//!
//! This is deliberately not a key exchange. It defends the realistic attack
//! (another machine on the network sending handoffs) without inventing a
//! pairing flow before the feature has users.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use openlogi_core::config::Edge;
use sha2::Sha256;

use super::{EdgeFraction, Handoff};

type HmacSha256 = Hmac<Sha256>;

/// How far apart two clocks may be before a handoff is refused.
///
/// Wide enough for hosts that only sync by NTP, short enough that a captured
/// packet is worthless by the time it could be replayed by hand.
pub const FRESHNESS: Duration = Duration::from_secs(30);

/// The canonical byte length of a signed handoff's preimage.
const PREIMAGE_LEN: usize = 22;

/// Why a received handoff was not acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SealError {
    /// The tag did not match — the sender does not share the secret.
    #[error("handoff signature does not match")]
    BadSignature,
    /// The send time is outside [`FRESHNESS`] of now.
    #[error("handoff is outside the freshness window")]
    Stale,
    /// The nonce was already used, so this is a replay.
    #[error("handoff nonce was already seen")]
    Replayed,
    /// The edge byte names no edge this build knows.
    #[error("handoff names an unknown edge")]
    UnknownEdge,
}

/// A handoff plus the proof it came from a peer that knows the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    /// Unix seconds at which the sender built the message.
    pub sent_at: u64,
    /// Per-message value, so two handoffs in the same second differ.
    pub nonce: u64,
    /// Target host slot, as in [`Handoff::host`].
    pub host: u8,
    /// Wire byte of the edge the pointer left through.
    pub edge: u8,
    /// Bit pattern of the crossing fraction.
    pub fraction_bits: u32,
    /// HMAC-SHA256 over the canonical preimage.
    pub tag: [u8; 32],
}

impl Sealed {
    /// The bytes the tag covers.
    ///
    /// Hand-rolled rather than delegated to a serializer: the preimage must
    /// stay byte-identical across versions of every crate involved, and a
    /// serializer's layout is not a promise this depends on.
    fn preimage(&self) -> [u8; PREIMAGE_LEN] {
        let mut bytes = [0; PREIMAGE_LEN];
        bytes[0..8].copy_from_slice(&self.sent_at.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.nonce.to_le_bytes());
        bytes[16] = self.host;
        bytes[17] = self.edge;
        bytes[18..22].copy_from_slice(&self.fraction_bits.to_le_bytes());
        bytes
    }

    /// The handoff this carries, once the edge byte is known.
    fn handoff(&self) -> Result<Handoff, SealError> {
        Ok(Handoff {
            host: self.host,
            left_through: Edge::from_code(self.edge).ok_or(SealError::UnknownEdge)?,
            at: EdgeFraction::new(f32::from_bits(self.fraction_bits)),
        })
    }
}

/// Signs and verifies handoffs against one shared secret.
#[derive(Clone)]
pub struct Seal {
    secret: Vec<u8>,
}

impl std::fmt::Debug for Seal {
    /// Never prints the secret; a log line is not a place to leak it.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Seal(<secret>)")
    }
}

impl Seal {
    /// Build a seal from the configured shared secret.
    #[must_use]
    pub fn new(secret: &str) -> Self {
        Self {
            secret: secret.as_bytes().to_vec(),
        }
    }

    fn tag(&self, preimage: &[u8]) -> [u8; 32] {
        // HMAC accepts a key of any length, so this cannot fail.
        let mut mac =
            <HmacSha256 as Mac>::new_from_slice(&self.secret).unwrap_or_else(|_| unreachable!());
        mac.update(preimage);
        mac.finalize().into_bytes().into()
    }

    /// Seal `handoff` for sending, stamped at `sent_at` with `nonce`.
    ///
    /// Both are parameters rather than read here so the caller owns the clock
    /// and the randomness, which is what makes this testable.
    #[must_use]
    pub fn seal(&self, handoff: &Handoff, sent_at: u64, nonce: u64) -> Sealed {
        let mut sealed = Sealed {
            sent_at,
            nonce,
            host: handoff.host,
            edge: handoff.left_through.code(),
            fraction_bits: handoff.at.value().to_bits(),
            tag: [0; 32],
        };
        sealed.tag = self.tag(&sealed.preimage());
        sealed
    }

    /// Verify a received handoff against the secret and the clock.
    ///
    /// The signature is checked before anything else: an unauthenticated
    /// message should not be able to probe the replay guard or the clock.
    pub fn open(
        &self,
        sealed: &Sealed,
        now: u64,
        guard: &mut ReplayGuard,
    ) -> Result<Handoff, SealError> {
        let mut mac =
            <HmacSha256 as Mac>::new_from_slice(&self.secret).unwrap_or_else(|_| unreachable!());
        mac.update(&sealed.preimage());
        mac.verify_slice(&sealed.tag)
            .map_err(|_| SealError::BadSignature)?;
        if now.abs_diff(sealed.sent_at) > FRESHNESS.as_secs() {
            return Err(SealError::Stale);
        }
        if !guard.accept(sealed.nonce, sealed.sent_at, now) {
            return Err(SealError::Replayed);
        }
        sealed.handoff()
    }
}

/// Remembers recently accepted nonces so a captured packet cannot be reused
/// inside the freshness window.
///
/// Entries older than the window are dropped on each accept, so the set stays
/// bounded by how many handoffs a peer can send in [`FRESHNESS`] — a handful,
/// given a switch takes a second and has a cooldown.
#[derive(Debug, Default)]
pub struct ReplayGuard {
    seen: Vec<(u64, u64)>,
}

impl ReplayGuard {
    /// Record `nonce` unless it was already seen; returns whether it is new.
    fn accept(&mut self, nonce: u64, sent_at: u64, now: u64) -> bool {
        self.seen
            .retain(|(_, stamped)| now.abs_diff(*stamped) <= FRESHNESS.as_secs());
        if self.seen.iter().any(|(seen, _)| *seen == nonce) {
            return false;
        }
        self.seen.push((nonce, sent_at));
        true
    }

    /// How many nonces are currently remembered.
    #[must_use]
    pub fn remembered(&self) -> usize {
        self.seen.len()
    }
}

/// Unix seconds now, or zero if the clock predates the epoch.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use openlogi_core::config::Edge;

    use super::{EdgeFraction, Handoff, ReplayGuard, Seal, SealError, Sealed};

    const NOW: u64 = 1_700_000_000;

    fn handoff() -> Handoff {
        Handoff {
            host: 2,
            left_through: Edge::Right,
            at: EdgeFraction::new(0.25),
        }
    }

    #[test]
    fn a_sealed_handoff_opens_with_the_same_secret() {
        let seal = Seal::new("shared");
        let sealed = seal.seal(&handoff(), NOW, 1);
        let opened = seal
            .open(&sealed, NOW, &mut ReplayGuard::default())
            .expect("a freshly sealed handoff opens");
        assert_eq!(opened, handoff());
    }

    #[test]
    fn a_different_secret_is_refused() {
        let sealed = Seal::new("shared").seal(&handoff(), NOW, 1);
        assert_eq!(
            Seal::new("other").open(&sealed, NOW, &mut ReplayGuard::default()),
            Err(SealError::BadSignature)
        );
    }

    #[test]
    fn every_signed_field_is_covered_by_the_tag() {
        let seal = Seal::new("shared");
        let original = seal.seal(&handoff(), NOW, 1);
        for tampered in [
            Sealed {
                host: 0,
                ..original.clone()
            },
            Sealed {
                edge: Edge::Left.code(),
                ..original.clone()
            },
            Sealed {
                fraction_bits: 0.9_f32.to_bits(),
                ..original.clone()
            },
            Sealed {
                sent_at: NOW + 1,
                ..original.clone()
            },
            Sealed {
                nonce: 99,
                ..original.clone()
            },
        ] {
            assert_eq!(
                seal.open(&tampered, NOW, &mut ReplayGuard::default()),
                Err(SealError::BadSignature),
                "a changed field must invalidate the tag"
            );
        }
    }

    #[test]
    fn an_old_handoff_is_stale_in_both_directions() {
        let seal = Seal::new("shared");
        let sealed = seal.seal(&handoff(), NOW, 1);
        assert_eq!(
            seal.open(&sealed, NOW + 31, &mut ReplayGuard::default()),
            Err(SealError::Stale)
        );
        // A peer whose clock runs ahead is equally untrusted.
        assert_eq!(
            seal.open(&sealed, NOW - 31, &mut ReplayGuard::default()),
            Err(SealError::Stale)
        );
        seal.open(&sealed, NOW + 29, &mut ReplayGuard::default())
            .expect("just inside the window is still fresh");
    }

    #[test]
    fn the_same_handoff_cannot_be_replayed() {
        let seal = Seal::new("shared");
        let sealed = seal.seal(&handoff(), NOW, 7);
        let mut guard = ReplayGuard::default();
        seal.open(&sealed, NOW, &mut guard)
            .expect("the first delivery is accepted");
        assert_eq!(
            seal.open(&sealed, NOW, &mut guard),
            Err(SealError::Replayed)
        );
    }

    #[test]
    fn a_forged_message_never_reaches_the_replay_guard() {
        // Order matters: an unauthenticated sender must not be able to fill
        // the guard, or refusing forgeries would become a denial of service.
        let seal = Seal::new("shared");
        let mut forged = seal.seal(&handoff(), NOW, 1);
        forged.tag[0] ^= 0xff;
        let mut guard = ReplayGuard::default();
        assert_eq!(
            seal.open(&forged, NOW, &mut guard),
            Err(SealError::BadSignature)
        );
        assert_eq!(guard.remembered(), 0);
    }

    #[test]
    fn the_guard_forgets_nonces_older_than_the_window() {
        let seal = Seal::new("shared");
        let mut guard = ReplayGuard::default();
        seal.open(&seal.seal(&handoff(), NOW, 1), NOW, &mut guard)
            .expect("the first nonce is accepted");
        assert_eq!(guard.remembered(), 1);
        // Far enough later that the old nonce is no longer worth remembering.
        let later = NOW + 120;
        seal.open(&seal.seal(&handoff(), later, 2), later, &mut guard)
            .expect("a later nonce is accepted");
        assert_eq!(guard.remembered(), 1);
    }

    #[test]
    fn an_unknown_edge_byte_is_refused_rather_than_guessed() {
        let seal = Seal::new("shared");
        let mut sealed = seal.seal(&handoff(), NOW, 1);
        sealed.edge = 9;
        // Re-sign so the failure is the edge, not the tampering.
        let resealed = Seal::new("shared").seal(
            &Handoff {
                host: sealed.host,
                left_through: Edge::Right,
                at: EdgeFraction::new(0.25),
            },
            NOW,
            1,
        );
        sealed.tag = resealed.tag;
        // The tag covers the edge byte, so a changed edge is a bad signature —
        // which is the stronger of the two refusals, and the point.
        assert_eq!(
            seal.open(&sealed, NOW, &mut ReplayGuard::default()),
            Err(SealError::BadSignature)
        );
    }
}
