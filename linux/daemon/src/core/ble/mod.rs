//! BLE constants, advertisement payload codec, and platform-side BLE
//! integration per spec §5 and §10.

// `frame` is the WIRE PROTOCOL: frame types, subtypes and chunk headers, shared
// byte-for-byte with the phone and used by the LAN transport too (see
// `lan::tcp_client`, which reassembles bulk-sync datasets by BLE frame type).
// It is pure Rust and must build everywhere — the phone cannot tell the two
// laptops apart, and `shared/vectors/` exists to keep it that way.
pub mod frame;

// The post-handshake event stream: AEAD-sealed frames in both directions over
// the AUDIO_SIGNAL characteristic, with nonce resync when the link drops one.
// Platform-neutral — it speaks `core::platform::GattLink`, so the same dispatch
// and the same resync run over BlueZ, over WinRT, or over a test fake.
pub mod audio_signal;

// Everything below is BlueZ over D-Bus: the central-role transport that
// carries those frames on Linux. A second OS brings its own transport (WinRT
// `BluetoothLEDevice`) behind `core::platform::BleCentral` and reuses both
// `frame` and `audio_signal` unchanged.
#[cfg(target_os = "linux")]
pub mod client;
#[cfg(target_os = "linux")]
pub mod scanner;

/// V1 protocol version byte. Receivers MUST reject other versions (§5.2).
pub const V1_VERSION: u8 = 0x01;

// ---- BINDING CONTRACT — wire-level, MUST match the peer ----
//
// The GATT UUIDs below are the on-air contract: the Android side mirrors
// them byte-for-byte in `a3/.../core/ble/Constants.kt`, and both derive
// from spec §10.1. Changing any value here is a protocol break
// — you MUST update the spec AND the Kotlin mirror in the same change, or
// the two stop discovering each other. The `uuid_contract_pins` test below
// is the tripwire that fails if a value drifts unintentionally.
//
// (Contrast the timeout / retry constants in `audio_orchestrator.rs`,
// which are local tuning and intentionally differ per side.)

/// Vortex Service UUID (§10.1).
pub const VORTEX_SERVICE_UUID: uuid::Uuid =
    uuid::uuid!("53ffc983-45f6-4891-a826-094ac749c063");

/// Pairing Control characteristic UUID (§10.1).
pub const PAIRING_CONTROL_UUID: uuid::Uuid =
    uuid::uuid!("b68f4442-adad-4d7c-a944-95c57b5094c7");

/// Reconnect Control characteristic UUID (§10.1).
pub const RECONNECT_CONTROL_UUID: uuid::Uuid =
    uuid::uuid!("bd2e76f0-d216-4f3b-9704-70cc272c3072");

/// Capability characteristic UUID (§10.1).
pub const CAPABILITY_UUID: uuid::Uuid =
    uuid::uuid!("e78510a3-b39e-459f-8957-864cdb301282");

/// Audio-signal characteristic UUID (Phase 2). Carries AEAD-protected
/// AUDIO_OP frames pushed by the phone as soon as a call comes in (or a
/// manual swap is requested), so handoff happens in ~200 ms instead of
/// waiting for the LAN heartbeat. Linux subscribes post-trust and
/// decrypts incoming frames with the same Noise transport keys that the
/// LAN path uses — no new crypto, no pairing touched.
pub const AUDIO_SIGNAL_UUID: uuid::Uuid =
    uuid::uuid!("c2e1c97f-3a4b-4d7e-9f0c-1e6a8b3d9c5f");

/// V1 service-data payload size (§5.1.3).
pub const ADV_PAYLOAD_LEN: usize = 10;

/// Flag bits in the 10-byte service-data payload (§5.1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvFlags(pub u8);

impl AdvFlags {
    pub const PAIRABLE: u8 = 0b0000_0001; // bit 0
    pub const TRUSTED_PRESENCE: u8 = 0b0000_0010; // bit 1
    pub const RESERVED_MASK: u8 = 0b1111_1100; // bits 2-7

    pub fn pairable() -> Self {
        Self(Self::PAIRABLE)
    }
    pub fn trusted_presence() -> Self {
        Self(Self::TRUSTED_PRESENCE)
    }
    pub fn is_pairable(self) -> bool {
        self.0 & Self::PAIRABLE != 0
    }
    pub fn is_trusted_presence(self) -> bool {
        self.0 & Self::TRUSTED_PRESENCE != 0
    }
    /// True iff the byte is a valid V1 flags byte:
    /// - all reserved bits zero,
    /// - exactly one of bit0 / bit1 set.
    pub fn is_well_formed(self) -> bool {
        if self.0 & Self::RESERVED_MASK != 0 {
            return false;
        }
        let lo = self.0 & 0b11;
        lo == Self::PAIRABLE || lo == Self::TRUSTED_PRESENCE
    }
}

/// Decoded V1 service-data payload (§5.1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvPayload {
    pub version: u8,
    pub flags: AdvFlags,
    /// 8-byte payload: pairing-window instance ID OR truncated presence token.
    /// Interpretation depends on `flags`.
    pub payload_8: [u8; 8],
}

impl AdvPayload {
    /// Encode into 10 bytes. Used by the advertiser.
    pub fn encode(&self) -> [u8; ADV_PAYLOAD_LEN] {
        let mut out = [0u8; ADV_PAYLOAD_LEN];
        out[0] = self.version;
        out[1] = self.flags.0;
        out[2..].copy_from_slice(&self.payload_8);
        out
    }

    /// Decode + validate per §5.2 filter pipeline.
    pub fn decode(bytes: &[u8]) -> Result<Self, AdvDecodeError> {
        if bytes.len() != ADV_PAYLOAD_LEN {
            return Err(AdvDecodeError::WrongLength(bytes.len()));
        }
        if bytes[0] != V1_VERSION {
            return Err(AdvDecodeError::WrongVersion(bytes[0]));
        }
        let flags = AdvFlags(bytes[1]);
        if !flags.is_well_formed() {
            return Err(AdvDecodeError::BadFlags(bytes[1]));
        }
        let mut payload_8 = [0u8; 8];
        payload_8.copy_from_slice(&bytes[2..]);
        Ok(Self {
            version: bytes[0],
            flags,
            payload_8,
        })
    }

    /// Build a pairable advert payload for a fresh pairing window.
    pub fn pairable(instance_id: [u8; 8]) -> Self {
        Self {
            version: V1_VERSION,
            flags: AdvFlags::pairable(),
            payload_8: instance_id,
        }
    }

    /// Build a trusted-presence advert payload from a presence token.
    pub fn trusted_presence(token: [u8; 8]) -> Self {
        Self {
            version: V1_VERSION,
            flags: AdvFlags::trusted_presence(),
            payload_8: token,
        }
    }
}

/// Decode a **Service Data — 128-bit UUID** AD section (type `0x21`): sixteen
/// bytes of service UUID followed by the service data itself.
///
/// Returns the payload only when the UUID is ours AND the payload passes the
/// §5.2 filter. `None` covers a foreign advert, a truncated section, and a
/// malformed payload alike, because a scanner's only useful question is "is
/// this a Vortex peer worth looking at".
///
/// The UUID in an AD structure is **little-endian** — reversed from the printed
/// form. That reversal is the whole reason this is a function with a test
/// rather than a comparison written inline at a call site.
///
/// Platforms that hand back parsed service data (BlueZ gives a UUID→bytes map)
/// go straight to [`AdvPayload::decode`]; this is for the ones that hand over
/// raw AD sections, as WinRT does.
pub fn decode_service_data_128(section: &[u8]) -> Option<AdvPayload> {
    if section.len() < 16 {
        return None;
    }
    let (uuid_le, payload) = section.split_at(16);
    let mut be = [0u8; 16];
    for (i, b) in uuid_le.iter().rev().enumerate() {
        be[i] = *b;
    }
    if uuid::Uuid::from_bytes(be) != VORTEX_SERVICE_UUID {
        return None;
    }
    AdvPayload::decode(payload).ok()
}

/// AD type for Service Data with a 128-bit UUID (Core Spec Supplement §1.11).
pub const AD_TYPE_SERVICE_DATA_128: u8 = 0x21;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvDecodeError {
    WrongLength(usize),
    WrongVersion(u8),
    BadFlags(u8),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift tripwire for the BINDING CONTRACT (§10.1). These literals are
    /// duplicated in `a3/.../core/ble/Constants.kt`; if you change one
    /// here, this test fails — a deliberate nudge to update the spec and
    /// the Kotlin mirror in the same change. The values are the wire
    /// contract: two builds with mismatched UUIDs never find each other.
    #[test]
    fn uuid_contract_pins() {
        assert_eq!(V1_VERSION, 0x01);
        assert_eq!(
            VORTEX_SERVICE_UUID.to_string(),
            "53ffc983-45f6-4891-a826-094ac749c063"
        );
        assert_eq!(
            PAIRING_CONTROL_UUID.to_string(),
            "b68f4442-adad-4d7c-a944-95c57b5094c7"
        );
        assert_eq!(
            RECONNECT_CONTROL_UUID.to_string(),
            "bd2e76f0-d216-4f3b-9704-70cc272c3072"
        );
        assert_eq!(
            CAPABILITY_UUID.to_string(),
            "e78510a3-b39e-459f-8957-864cdb301282"
        );
        assert_eq!(
            AUDIO_SIGNAL_UUID.to_string(),
            "c2e1c97f-3a4b-4d7e-9f0c-1e6a8b3d9c5f"
        );
    }

    #[test]
    fn pairable_round_trip() {
        let id: [u8; 8] = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let p = AdvPayload::pairable(id);
        let encoded = p.encode();
        assert_eq!(encoded.len(), 10);
        assert_eq!(encoded[0], 0x01);
        assert_eq!(encoded[1], 0x01); // pairable bit
        assert_eq!(&encoded[2..], &id);

        let decoded = AdvPayload::decode(&encoded).unwrap();
        assert_eq!(decoded, p);
        assert!(decoded.flags.is_pairable());
        assert!(!decoded.flags.is_trusted_presence());
    }

    #[test]
    fn trusted_presence_round_trip() {
        let token: [u8; 8] = [0xAA; 8];
        let p = AdvPayload::trusted_presence(token);
        let encoded = p.encode();
        assert_eq!(encoded[1], 0x02);

        let decoded = AdvPayload::decode(&encoded).unwrap();
        assert!(decoded.flags.is_trusted_presence());
        assert!(!decoded.flags.is_pairable());
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(matches!(
            AdvPayload::decode(&[0u8; 9]),
            Err(AdvDecodeError::WrongLength(9))
        ));
        assert!(matches!(
            AdvPayload::decode(&[0u8; 11]),
            Err(AdvDecodeError::WrongLength(11))
        ));
    }

    #[test]
    fn rejects_wrong_version() {
        let mut bytes = AdvPayload::pairable([0; 8]).encode();
        bytes[0] = 0x02;
        assert!(matches!(
            AdvPayload::decode(&bytes),
            Err(AdvDecodeError::WrongVersion(2))
        ));
    }

    #[test]
    fn rejects_reserved_bits_set() {
        let mut bytes = AdvPayload::pairable([0; 8]).encode();
        bytes[1] = 0x05; // bit0 + bit2 (reserved)
        assert!(matches!(
            AdvPayload::decode(&bytes),
            Err(AdvDecodeError::BadFlags(0x05))
        ));
    }

    #[test]
    fn rejects_both_modes_set() {
        let mut bytes = AdvPayload::pairable([0; 8]).encode();
        bytes[1] = 0x03; // bit0 + bit1 — mutually exclusive
        assert!(matches!(
            AdvPayload::decode(&bytes),
            Err(AdvDecodeError::BadFlags(0x03))
        ));
    }

    /// Build the AD section a phone actually emits: little-endian UUID, then
    /// the 10-byte payload.
    fn service_data_section(payload: [u8; ADV_PAYLOAD_LEN]) -> Vec<u8> {
        let mut v: Vec<u8> = VORTEX_SERVICE_UUID.as_bytes().iter().rev().copied().collect();
        v.extend_from_slice(&payload);
        v
    }

    #[test]
    fn decodes_a_service_data_section_from_raw_ad_bytes() {
        let token = [0x11u8; 8];
        let section = service_data_section(AdvPayload::trusted_presence(token).encode());
        let decoded = decode_service_data_128(&section).expect("ours");
        assert!(decoded.flags.is_trusted_presence());
        assert_eq!(decoded.payload_8, token);
    }

    /// The UUID is little-endian on air. Feeding it big-endian must NOT match,
    /// or a scanner would silently depend on which way round the platform
    /// happened to hand the bytes over.
    #[test]
    fn a_big_endian_uuid_does_not_match() {
        let mut section: Vec<u8> = VORTEX_SERVICE_UUID.as_bytes().to_vec();
        section.extend_from_slice(&AdvPayload::pairable([0; 8]).encode());
        assert!(decode_service_data_128(&section).is_none());
    }

    #[test]
    fn rejects_foreign_short_and_malformed_sections() {
        // Someone else's service data.
        let mut foreign: Vec<u8> = uuid::uuid!("00001234-0000-1000-8000-00805f9b34fb")
            .as_bytes()
            .iter()
            .rev()
            .copied()
            .collect();
        foreign.extend_from_slice(&AdvPayload::pairable([0; 8]).encode());
        assert!(decode_service_data_128(&foreign).is_none());

        // Truncated before the UUID even ends.
        assert!(decode_service_data_128(&[0u8; 8]).is_none());

        // Ours, but the payload fails the §5.2 filter (both mode bits set).
        let mut bad = AdvPayload::pairable([0; 8]).encode();
        bad[1] = 0x03;
        assert!(decode_service_data_128(&service_data_section(bad)).is_none());
    }

    #[test]
    fn rejects_no_mode_set() {
        let mut bytes = AdvPayload::pairable([0; 8]).encode();
        bytes[1] = 0x00; // neither bit0 nor bit1
        assert!(matches!(
            AdvPayload::decode(&bytes),
            Err(AdvDecodeError::BadFlags(0x00))
        ));
    }
}
