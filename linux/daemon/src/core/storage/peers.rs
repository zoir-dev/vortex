//! Trusted Peer storage per spec §3.3 (V1 subset).
//!
//! V1 schema (encoded as a 72-byte blob):
//!   | 0  | 32 | peer_static_pub |
//!   | 32 | 32 | prs (secret)    |
//!   | 64 |  8 | paired_at u64 BE |
//!
//! For Phase 5d.1 we persist `peer_static_pub` + `prs` + `paired_at`. The
//! richer fields (`peer_id`, `peer_display_name`, `peer_platform`) require
//! the post-handshake identity exchange (§6.4.4 / §6.7.1) which is
//! deferred.

use super::{StorageError, StorageResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedPeer {
    pub peer_static_pub: [u8; 32],
    pub prs: [u8; 32],
    pub paired_at: u64,
    /// Friendly device name exchanged during pairing. Absent on
    /// records created before the name-exchange landed.
    pub peer_name: Option<String>,
}

impl TrustedPeer {
    /// Encode as 72 bytes (v1) when no name is present, or 72 + utf8
    /// name bytes (v2). Backward-compatible: v1 readers reject longer
    /// records, but we never write v2 unless `peer_name` is Some.
    pub fn encode(&self) -> Vec<u8> {
        let name_bytes = self.peer_name.as_deref().unwrap_or("").as_bytes();
        let mut out = Vec::with_capacity(72 + name_bytes.len());
        out.extend_from_slice(&self.peer_static_pub);
        out.extend_from_slice(&self.prs);
        out.extend_from_slice(&self.paired_at.to_be_bytes());
        if !name_bytes.is_empty() {
            out.extend_from_slice(name_bytes);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> StorageResult<Self> {
        if bytes.len() < 72 {
            return Err(StorageError::Backend(format!(
                "trusted peer record too short: {} (need ≥72)",
                bytes.len()
            )));
        }
        let mut peer_static_pub = [0u8; 32];
        peer_static_pub.copy_from_slice(&bytes[0..32]);
        let mut prs = [0u8; 32];
        prs.copy_from_slice(&bytes[32..64]);
        let paired_at = u64::from_be_bytes(bytes[64..72].try_into().unwrap());
        // Sanitize on decode (ChatGPT review #9). Writers go through
        // `sanitize_peer_name`, but older / corrupted records (e.g.
        // pre-sanitizer build, on-disk corruption, future schema
        // skew) could still contain control chars or oversized
        // strings that the UI would render badly. Strip both here
        // so every read produces UI-safe text — defense in depth.
        let peer_name = if bytes.len() > 72 {
            std::str::from_utf8(&bytes[72..])
                .ok()
                .map(crate::core::pairing::handshake::sanitize_peer_name)
                .filter(|s| !s.is_empty())
        } else {
            None
        };
        Ok(Self {
            peer_static_pub,
            prs,
            paired_at,
            peer_name,
        })
    }
}

pub trait PeerStore: Send + Sync {
    fn save(&self, peer: &TrustedPeer) -> StorageResult<()>;
    fn load(&self, peer_static_pub: &[u8; 32]) -> StorageResult<TrustedPeer>;
    fn list(&self) -> StorageResult<Vec<TrustedPeer>>;
    fn forget(&self, peer_static_pub: &[u8; 32]) -> StorageResult<()>;

    /// Per-peer monotonic reconnect counter (M6 audit fix). Defaults to
    /// zero before the first successful reconnect.
    ///
    /// On each successful IK exchange both sides bump and persist
    /// `max(local, peer_seen) + 1`. A peer counter strictly lower than
    /// the local counter is a backup-restore replay signal — logged at
    /// warn level; not hard-rejected because legitimate partial
    /// reconnects can desync. See trait doc on the Android side for
    /// the full rationale.
    fn load_counter(&self, _peer_static_pub: &[u8; 32]) -> StorageResult<u64> {
        Ok(0)
    }
    fn bump_counter(&self, _peer_static_pub: &[u8; 32], _peer_seen: u64) -> StorageResult<u64> {
        Ok(0)
    }

    /// BT-bonded identity address for this peer (BD_ADDR string), set
    /// after a successful bond during pairing. `None` until bonded. The
    /// persistent reconnect loop uses it to connect DIRECTLY (no BLE
    /// scan — scanning conflicts with A2DP on the shared radio, which is
    /// what made the handoff link flaky while audio streamed).
    fn load_bonded_addr(&self, _peer_static_pub: &[u8; 32]) -> StorageResult<Option<String>> {
        Ok(None)
    }
    fn save_bonded_addr(&self, _peer_static_pub: &[u8; 32], _addr: &str) -> StorageResult<()> {
        Ok(())
    }

    /// Atomically read-and-increment the outbound audio-op nonce for
    /// `peer_static_pub`. Returns the new value to embed in the next
    /// `AudioOpFrame`. Default 0 → first send carries nonce 1.
    fn next_audio_out_nonce(&self, _peer_static_pub: &[u8; 32]) -> StorageResult<u64> {
        Ok(0)
    }

    /// Highest inbound nonce we've accepted from this peer. The
    /// orchestrator rejects any incoming frame with `nonce <= this`.
    fn load_audio_in_nonce(&self, _peer_static_pub: &[u8; 32]) -> StorageResult<u64> {
        Ok(0)
    }

    /// Atomically accept `nonce` if and only if it's strictly greater
    /// than the previously-seen value. Returns Ok(true) when the
    /// caller may go on to dispatch the frame (we committed the new
    /// nonce); Ok(false) when the frame is a replay and must be
    /// dropped.
    ///
    /// This is the API that defends against the BLE+LAN duplicate-
    /// delivery race: both transports can hand the orchestrator the
    /// same frame in the same millisecond, and the prior `load + if
    /// nonce > seen + commit` shape let both readers see the old
    /// `seen` value and both pass the check. Implementations override
    /// this with a single mutex window around load-compare-commit so
    /// exactly one caller wins.
    ///
    /// Default impl exists for the no-op / in-memory stores — they
    /// don't have a shared mutex, so the default just does the same
    /// load+commit pair the orchestrator was doing inline. Real
    /// stores MUST override.
    fn try_accept_audio_in_nonce(
        &self,
        peer_static_pub: &[u8; 32],
        nonce: u64,
    ) -> StorageResult<bool> {
        let seen = self.load_audio_in_nonce(peer_static_pub)?;
        if nonce <= seen {
            return Ok(false);
        }
        self.commit_audio_in_nonce(peer_static_pub, nonce)?;
        Ok(true)
    }

    /// Persist a new accepted inbound nonce. The implementation MUST
    /// `max()` against the current value to be idempotent under
    /// concurrent frame arrival (BLE + LAN can both deliver the same
    /// frame; we accept whichever lands first and ignore the other).
    fn commit_audio_in_nonce(&self, _peer_static_pub: &[u8; 32], _nonce: u64) -> StorageResult<()> {
        Ok(())
    }
}

/// Runs all Secret Service work on the dedicated storage runtime via
/// [`super::secret_block_on`] (same approach as
/// [`super::secret_service::SecretServiceIdentityStore`]) — never on the
/// ambient runtime, whose workers a call-time burst can fully park (the
/// 2026-06-11 earbuds-switch freeze).
///
/// A `Mutex` serializes every call so a burst of concurrent saves /
/// reconnect attempts cannot leave the Secret Service with duplicate
/// items for the same peer_static_pub or race the create-then-search
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        let peer = TrustedPeer {
            peer_static_pub: [0xAA; 32],
            prs: [0xBB; 32],
            paired_at: 1_700_000_000,
            peer_name: None,
        };
        let encoded = peer.encode();
        assert_eq!(encoded.len(), 72);
        let decoded = TrustedPeer::decode(&encoded).unwrap();
        assert_eq!(decoded, peer);
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(TrustedPeer::decode(&[0u8; 71]).is_err());
        // ≥72 bytes is accepted — bytes past 72 are treated as the
        // optional utf-8 device name (v2 format).
        assert!(TrustedPeer::decode(&[0u8; 72]).is_ok());
    }

    #[test]
    fn round_trip_with_name() {
        let peer = TrustedPeer {
            peer_static_pub: [0xAA; 32],
            prs: [0xBB; 32],
            paired_at: 1_700_000_000,
            peer_name: Some("zoyirjon-Blade".to_string()),
        };
        let encoded = peer.encode();
        let decoded = TrustedPeer::decode(&encoded).unwrap();
        assert_eq!(decoded, peer);
        assert_eq!(decoded.peer_name.as_deref(), Some("zoyirjon-Blade"));
    }
}
