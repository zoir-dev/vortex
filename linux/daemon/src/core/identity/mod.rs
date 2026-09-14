//! Local Vortex identity per spec §3.1 and §3.2.
//!
//! Per spec, every Device, on first launch, generates one Identity Record:
//!
//! | version | device_id | static_priv | static_pub | created_at | platform |
//! | u8 (1)  | 16 bytes  | 32 bytes    | 32 bytes   | u64 BE (8) | u8 (1)   |
//!
//! `static_priv` MUST be persisted via platform secure storage. Other fields
//! MAY live in plaintext local storage.

use rand::RngCore;
use serde::{Deserialize, Serialize};

use super::crypto::x25519::{public_from_private, X25519Pub, X25519Sec, X25519SecBytes};

/// V1 identity record version byte.
pub const IDENTITY_VERSION: u8 = 0x01;

/// `platform` byte (§3.1).
///
/// LOCAL, despite living in a record with a spec section: the identity record
/// is written to this device's own secure storage and never leaves it. What the
/// peer sees is the `class` STRING in `AppState` ("laptop", "phone"), decoded by
/// its own `DeviceClass` — the phone's `Platform.fromByte` only ever parses the
/// record IT stored. So adding a variant here is not a wire change and cannot
/// make an existing phone reject us; the only reader of a `0x03` byte is a
/// Windows build reading back its own record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Android = 0x01,
    Linux = 0x02,
    Windows = 0x03,
}

impl Platform {
    pub fn as_byte(self) -> u8 {
        self as u8
    }
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Android),
            0x02 => Some(Self::Linux),
            0x03 => Some(Self::Windows),
            _ => None,
        }
    }
}

/// One Vortex device identity (§3.1).
#[derive(Debug, Clone)]
pub struct IdentityRecord {
    pub version: u8,
    pub device_id: [u8; 16],
    pub static_priv: X25519Sec,
    pub static_pub: X25519Pub,
    pub created_at: u64,
    pub platform: Platform,
}

impl IdentityRecord {
    /// Generate a fresh identity using a CSPRNG. The resulting Identity
    /// Record is a freshly minted, never-before-paired identity.
    pub fn generate(platform: Platform) -> Self {
        let mut rng = rand::rngs::OsRng;
        Self::generate_with_rng(platform, &mut rng)
    }

    /// Same as [`generate`] but with a caller-provided RNG (useful for
    /// deterministic tests; production MUST use `OsRng`).
    pub fn generate_with_rng<R: RngCore>(platform: Platform, rng: &mut R) -> Self {
        let mut device_id = [0u8; 16];
        rng.fill_bytes(&mut device_id);
        let mut priv_bytes = [0u8; 32];
        rng.fill_bytes(&mut priv_bytes);
        Self::from_private(platform, device_id, priv_bytes, current_unix_seconds())
    }

    /// Build an identity from an externally-supplied private scalar.
    /// Used by tests and by the vector generator.
    pub fn from_private(
        platform: Platform,
        device_id: [u8; 16],
        static_priv: X25519SecBytes,
        created_at: u64,
    ) -> Self {
        let static_pub = public_from_private(&static_priv);
        Self {
            version: IDENTITY_VERSION,
            device_id,
            static_priv: X25519Sec(static_priv),
            static_pub: X25519Pub(static_pub),
            created_at,
            platform,
        }
    }

    /// Encode as a 90-byte record per §3.1.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(90);
        out.push(self.version); //                  0..1   version
        out.extend_from_slice(&self.device_id); //  1..17  device_id
        out.extend_from_slice(&self.static_priv.0);// 17..49 static_priv
        out.extend_from_slice(&self.static_pub.0);//49..81 static_pub
        out.extend_from_slice(&self.created_at.to_be_bytes()); // 81..89 created_at
        out.push(self.platform.as_byte()); //       89..90 platform
        debug_assert_eq!(out.len(), 90);
        out
    }

    /// Decode a 90-byte record written by [`Self::encode`].
    ///
    /// Lives here, beside `encode`, rather than inside a storage backend: this
    /// is the on-disk identity format, every backend reads the same bytes, and
    /// a second copy of the offsets is a second place to get them wrong.
    ///
    /// Strict on length and version — a record that is not exactly what we
    /// wrote is a corrupt keypair, and guessing at it would mean running with
    /// a private key we cannot vouch for.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != 90 {
            return Err(format!(
                "stored identity has wrong length: {} (expected 90)",
                bytes.len()
            ));
        }
        let version = bytes[0];
        if version != IDENTITY_VERSION {
            return Err(format!(
                "stored identity has unknown version: {version:#04x}"
            ));
        }
        let mut device_id = [0u8; 16];
        device_id.copy_from_slice(&bytes[1..17]);
        let mut static_priv = [0u8; 32];
        static_priv.copy_from_slice(&bytes[17..49]);
        let mut static_pub = [0u8; 32];
        static_pub.copy_from_slice(&bytes[49..81]);
        let created_at = u64::from_be_bytes(bytes[81..89].try_into().unwrap());
        let platform = Platform::from_byte(bytes[89])
            .ok_or_else(|| format!("unknown platform byte: {}", bytes[89]))?;
        Ok(Self {
            version,
            device_id,
            static_priv: crate::core::crypto::x25519::X25519Sec(static_priv),
            static_pub: crate::core::crypto::x25519::X25519Pub(static_pub),
            created_at,
            platform,
        })
    }
}

fn current_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// JSON-friendly view used in CLI output and storage (excludes `static_priv`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityPublicView {
    pub version: u8,
    #[serde(with = "hex::serde")]
    pub device_id: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub static_pub: Vec<u8>,
    pub created_at: u64,
    pub platform: Platform,
}

impl From<&IdentityRecord> for IdentityPublicView {
    fn from(r: &IdentityRecord) -> Self {
        Self {
            version: r.version,
            device_id: r.device_id.to_vec(),
            static_pub: r.static_pub.0.to_vec(),
            created_at: r.created_at,
            platform: r.platform,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::crypto::x25519::X25519PubBytes;

    /// Deterministic identity from a known private scalar (RFC 7748 Alice key
    /// from §6.1, used in the wider Noise community as a smoke vector).
    /// We do NOT use this scalar in production; this is a test-only seed.
    const ALICE_PRIV: X25519SecBytes = [
        0x77, 0x07, 0x6d, 0x0a, 0x73, 0x18, 0xa5, 0x7d, 0x3c, 0x16, 0xc1, 0x72, 0x51, 0xb2, 0x66,
        0x45, 0xdf, 0x4c, 0x2f, 0x87, 0xeb, 0xc0, 0x99, 0x2a, 0xb1, 0x77, 0xfb, 0xa5, 0x1d, 0xb9,
        0x2c, 0x2a,
    ];
    const ALICE_PUB_EXPECTED: X25519PubBytes = [
        0x85, 0x20, 0xf0, 0x09, 0x89, 0x30, 0xa7, 0x54, 0x74, 0x8b, 0x7d, 0xdc, 0xb4, 0x3e, 0xf7,
        0x5a, 0x0d, 0xbf, 0x3a, 0x0d, 0x26, 0x38, 0x1a, 0xf4, 0xeb, 0xa4, 0xa9, 0x8e, 0xaa, 0x9b,
        0x4e, 0x6a,
    ];

    #[test]
    fn rfc_7748_alice_pub_matches() {
        let id =
            IdentityRecord::from_private(Platform::Linux, [0u8; 16], ALICE_PRIV, 1_700_000_000);
        assert_eq!(id.static_pub.0, ALICE_PUB_EXPECTED);
    }

    #[test]
    fn encode_layout_is_90_bytes() {
        let id =
            IdentityRecord::from_private(Platform::Linux, [0u8; 16], ALICE_PRIV, 1_700_000_000);
        let bytes = id.encode();
        assert_eq!(bytes.len(), 90);
        assert_eq!(bytes[0], IDENTITY_VERSION);
        assert_eq!(&bytes[1..17], &[0u8; 16]);
        assert_eq!(&bytes[17..49], &ALICE_PRIV);
        assert_eq!(&bytes[49..81], &ALICE_PUB_EXPECTED);
        assert_eq!(
            u64::from_be_bytes(bytes[81..89].try_into().unwrap()),
            1_700_000_000,
        );
        assert_eq!(bytes[89], 0x02); // Linux
    }

    #[test]
    fn generated_identity_has_distinct_keys() {
        let a = IdentityRecord::generate(Platform::Linux);
        let b = IdentityRecord::generate(Platform::Linux);
        assert_ne!(a.device_id, b.device_id);
        assert_ne!(a.static_priv.0, b.static_priv.0);
        assert_ne!(a.static_pub.0, b.static_pub.0);
        assert_eq!(a.platform, Platform::Linux);
    }
}
