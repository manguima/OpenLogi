//! The bytes a sealed handoff travels as.
//!
//! A fixed 61-byte frame rather than a serializer: both ends must agree on it
//! across independent builds, and a length-prefixed self-describing format
//! would buy nothing here — every field is a fixed-width integer, and the
//! preimage the signature covers is already laid out by hand.

use super::seal::Sealed;

/// Magic prefix, so a stray connection is rejected before anything is parsed.
const MAGIC: [u8; 4] = *b"OLFL";

/// Frame version. A peer speaking another one is refused rather than guessed
/// at: the pointer is not a thing to move on a maybe.
const VERSION: u8 = 1;

/// Total size of one framed handoff.
pub const FRAME_LEN: usize = 61;

/// Why a received frame could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// The frame is not the fixed length.
    #[error("handoff frame has the wrong length")]
    Length,
    /// The magic prefix is absent — this is not one of our frames.
    #[error("handoff frame is not an OpenLogi flow frame")]
    Magic,
    /// The peer speaks a frame version this build does not.
    #[error("handoff frame speaks version {0}, this build speaks {VERSION}")]
    Version(u8),
}

/// Lay `sealed` out as bytes.
#[must_use]
pub fn encode(sealed: &Sealed) -> [u8; FRAME_LEN] {
    let mut frame = [0; FRAME_LEN];
    frame[0..4].copy_from_slice(&MAGIC);
    frame[4] = VERSION;
    frame[5..13].copy_from_slice(&sealed.sent_at.to_le_bytes());
    frame[13..21].copy_from_slice(&sealed.nonce.to_le_bytes());
    frame[21] = sealed.host;
    frame[22] = sealed.edge;
    frame[23..27].copy_from_slice(&sealed.fraction_bits.to_le_bytes());
    frame[27..59].copy_from_slice(&sealed.tag);
    // 59..61 stay zero: room for a field that does not exist yet, so adding
    // one later does not have to move every offset above it.
    frame
}

/// Read a frame back, checking only its shape — the signature is the seal's
/// business, and this must not decide anything a forged frame could influence.
pub fn decode(bytes: &[u8]) -> Result<Sealed, FrameError> {
    let frame: &[u8; FRAME_LEN] = bytes.try_into().map_err(|_| FrameError::Length)?;
    if frame[0..4] != MAGIC {
        return Err(FrameError::Magic);
    }
    if frame[4] != VERSION {
        return Err(FrameError::Version(frame[4]));
    }
    let mut sent_at = [0; 8];
    sent_at.copy_from_slice(&frame[5..13]);
    let mut nonce = [0; 8];
    nonce.copy_from_slice(&frame[13..21]);
    let mut fraction_bits = [0; 4];
    fraction_bits.copy_from_slice(&frame[23..27]);
    let mut tag = [0; 32];
    tag.copy_from_slice(&frame[27..59]);
    Ok(Sealed {
        sent_at: u64::from_le_bytes(sent_at),
        nonce: u64::from_le_bytes(nonce),
        host: frame[21],
        edge: frame[22],
        fraction_bits: u32::from_le_bytes(fraction_bits),
        tag,
    })
}

#[cfg(test)]
mod tests {
    use openlogi_core::config::Edge;

    use super::super::seal::Seal;
    use super::super::{EdgeFraction, Handoff};
    use super::{FRAME_LEN, FrameError, decode, encode};

    fn sealed() -> super::Sealed {
        Seal::new("shared").seal(
            &Handoff {
                host: 2,
                left_through: Edge::Right,
                at: EdgeFraction::new(0.25),
            },
            1_700_000_000,
            42,
        )
    }

    #[test]
    fn a_frame_round_trips_every_field() {
        let original = sealed();
        let decoded = decode(&encode(&original)).expect("our own frame decodes");
        assert_eq!(decoded, original);
    }

    #[test]
    fn the_frame_is_the_documented_size() {
        assert_eq!(encode(&sealed()).len(), FRAME_LEN);
    }

    #[test]
    fn a_short_or_long_frame_is_refused() {
        let frame = encode(&sealed());
        assert_eq!(decode(&frame[..FRAME_LEN - 1]), Err(FrameError::Length));
        let mut long = frame.to_vec();
        long.push(0);
        assert_eq!(decode(&long), Err(FrameError::Length));
        assert_eq!(decode(&[]), Err(FrameError::Length));
    }

    #[test]
    fn something_that_is_not_our_frame_is_refused_before_parsing() {
        let mut frame = encode(&sealed());
        frame[0] = b'X';
        assert_eq!(decode(&frame), Err(FrameError::Magic));
    }

    #[test]
    fn another_frame_version_is_refused_rather_than_guessed() {
        let mut frame = encode(&sealed());
        frame[4] = 2;
        assert_eq!(decode(&frame), Err(FrameError::Version(2)));
    }

    #[test]
    fn decoding_never_validates_the_signature() {
        // Framing and authentication are separate jobs; a frame with a
        // worthless tag still decodes, and the seal is what rejects it.
        let mut original = sealed();
        original.tag = [0; 32];
        let decoded = decode(&encode(&original)).expect("shape is fine, tag is not");
        assert_eq!(decoded.tag, [0; 32]);
    }
}
