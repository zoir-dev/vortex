//! Target names and counter blobs for the Windows Credential Manager backend.
//!
//! Windows has no "attributes" the way Secret Service does — a generic
//! credential is just a target-name string and a blob — so the naming scheme
//! *is* the schema, and one of its rules is load-bearing: peer records and the
//! counters hung off them share a prefix, and telling them apart is the only
//! thing stopping `PeerStore::list` from returning a "peer" whose public key is
//! actually a reconnect counter.
//!
//! Compiled on every platform so that rule has tests that run on the machine
//! this is developed on, the same reasoning as `platform::toast_xml`.

use super::{StorageError, StorageResult};

/// Target name for the local identity record.
pub const IDENTITY_TARGET: &str = "Vortex/identity";
/// Prefix every trusted-peer credential shares.
pub const PEER_PREFIX: &str = "Vortex/peer/";

/// Sub-keys hung off a peer's target name. Separate credentials rather than
/// fields in one blob because each is written on its own schedule — the
/// counters and nonces are bumped constantly while the record itself almost
/// never changes, and a read-modify-write of a combined blob would be a race
/// between the heartbeat and the BLE loop.
pub const SUB_COUNTER: &str = "/counter";
pub const SUB_AUDIO_OUT: &str = "/audio-out-nonce";
pub const SUB_AUDIO_IN: &str = "/audio-in-nonce";
pub const SUB_BONDED: &str = "/bonded-addr";

/// The target name for a peer's main record.
pub fn peer_target(peer_static_pub: &[u8; 32]) -> String {
    format!("{PEER_PREFIX}{}", hex::encode(peer_static_pub))
}

/// Recover the public key from a target name produced by [`peer_target`].
///
/// `None` for anything that is not a bare peer record — in particular the
/// `/counter` and `/audio-*-nonce` children, which match the same
/// `Vortex/peer/*` enumeration filter and must never be mistaken for peers.
pub fn peer_from_target(target: &str) -> Option<[u8; 32]> {
    let rest = target.strip_prefix(PEER_PREFIX)?;
    // The children are what this guard is for. Windows' enumeration wildcard
    // cannot express "one path segment", so the filtering happens here.
    if rest.contains('/') {
        return None;
    }
    let bytes = hex::decode(rest).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    Some(arr)
}

/// A `u64` counter as stored: big-endian, matching every other integer this
/// project puts on a wire or on disk.
pub fn encode_u64(v: u64) -> Vec<u8> {
    v.to_be_bytes().to_vec()
}

/// Read a counter back.
///
/// A wrong-sized blob is an error, not a zero. These counters are replay
/// defences — the audio-op nonce and the reconnect counter — and silently
/// restarting one at zero would re-accept frames we have already seen.
pub fn decode_u64(bytes: &[u8]) -> StorageResult<u64> {
    let arr: [u8; 8] = bytes.try_into().map_err(|_| {
        StorageError::Backend(format!("counter blob is {} bytes, want 8", bytes.len()))
    })?;
    Ok(u64::from_be_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [0xAB; 32];

    #[test]
    fn a_peer_target_round_trips() {
        let t = peer_target(&KEY);
        assert!(t.starts_with(PEER_PREFIX));
        assert_eq!(peer_from_target(&t), Some(KEY));
    }

    /// The load-bearing rule: the children share the peer's prefix and would
    /// otherwise be listed as peers, each with a nonsense public key.
    #[test]
    fn the_children_of_a_peer_are_not_peers() {
        let base = peer_target(&KEY);
        for sub in [SUB_COUNTER, SUB_AUDIO_OUT, SUB_AUDIO_IN, SUB_BONDED] {
            let child = format!("{base}{sub}");
            assert!(
                peer_from_target(&child).is_none(),
                "{child} must not read as a peer"
            );
        }
    }

    #[test]
    fn foreign_and_malformed_targets_are_rejected() {
        assert_eq!(peer_from_target("Vortex/identity"), None);
        assert_eq!(peer_from_target("SomeoneElse/peer/aabb"), None);
        // Right prefix, not hex.
        assert_eq!(peer_from_target(&format!("{PEER_PREFIX}zzzz")), None);
        // Right prefix, valid hex, wrong length — a 31-byte key is not a key.
        assert_eq!(peer_from_target(&format!("{PEER_PREFIX}{}", "ab".repeat(31))), None);
        assert_eq!(peer_from_target(PEER_PREFIX), None);
    }

    #[test]
    fn counters_round_trip_big_endian() {
        for v in [0u64, 1, 42, u64::MAX] {
            assert_eq!(decode_u64(&encode_u64(v)).unwrap(), v);
        }
        assert_eq!(encode_u64(1), vec![0, 0, 0, 0, 0, 0, 0, 1]);
    }

    /// A truncated counter must not read as a smaller number, and an empty one
    /// must not read as zero: both would roll a replay defence backwards.
    #[test]
    fn a_wrong_sized_counter_is_an_error_not_a_zero() {
        assert!(decode_u64(&[]).is_err());
        assert!(decode_u64(&[0, 0, 0, 1]).is_err());
        assert!(decode_u64(&[0; 9]).is_err());
    }
}
