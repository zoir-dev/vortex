//! Vortex wire frame per spec §6.3.
//!
//! ```text
//! | 0 | 1     | type    |
//! | 1 | 1     | sub     |
//! | 2 | 2 BE  | length  |
//! | 4 | N     | payload |
//! ```
//!
//! Frames are length-prefixed with `length` ∈ 0..MAX_FRAME_PAYLOAD bytes.
//! Receivers MUST drop frames whose `length` exceeds the maximum or whose
//! buffer ends early.

/// Header size in bytes.
pub const FRAME_HEADER_LEN: usize = 4;

/// Per spec §11. Larger frames are a `bad-frame` error. Sized to admit a
/// 48 KiB LAN file-transfer chunk + AEAD tag (BLE notifies stay MTU-small
/// regardless; the `length` field is u16 so the hard ceiling is 65535).
pub const MAX_FRAME_PAYLOAD: usize = 63 * 1024;

/// Frame type constants (spec §6.3 + §9.3).
pub mod ty {
    pub const PAIRING_HANDSHAKE: u8 = 0x10;
    pub const PAIRING_APPROVAL: u8 = 0x11;
    pub const PAIRING_TRUSTED_INFO: u8 = 0x12;
    pub const RECONNECT_HANDSHAKE: u8 = 0x20;
    /// Reserved: V2 channel-promotion / join-proof (spec §8.5). V1 does
    /// NOT exchange this — each transport runs its own IK. Keeping the
    /// byte reserved so the V2 design space stays compatible.
    #[allow(dead_code)]
    pub const LAN_JOIN_PROOF_RESERVED_V2: u8 = 0x21;
    pub const TRANSPORT_KEEPALIVE: u8 = 0x30;
    pub const TRANSPORT_APP_DATA: u8 = 0x31;
    /// Post-handshake AEAD-wrapped earbuds-switch op frame. Carries an
    /// `AudioOpFrame` JSON inside (see `core::earbuds::AudioOpFrame`).
    /// Lives next to `TRANSPORT_APP_DATA` so switches don't have to
    /// piggyback on the 12 s heartbeat — they get their own narrow
    /// frame and the mDNS nudge wakes the peer immediately.
    pub const AUDIO_OP: u8 = 0x32;
    /// Post-handshake AEAD-wrapped app-state push. Carries an `AppState`
    /// JSON (same shape as `TRANSPORT_APP_DATA`) so a battery/charging
    /// change reaches the peer over the already-open BLE link in ~200 ms
    /// instead of waiting for the LAN heartbeat. Routed entirely
    /// separately from `AUDIO_OP` — it never touches the audio-handoff
    /// state machine — so the proven switch path is unaffected.
    pub const STATE: u8 = 0x33;
    /// Post-handshake AEAD-wrapped notification-mirror push. Carries a
    /// `NotificationMirror` JSON (app + title + text + ts) — a phone
    /// notification forwarded to the laptop for desktop display. Routed
    /// separately from AUDIO_OP/STATE; never touches the audio handoff.
    pub const NOTIFICATION: u8 = 0x34;
    /// Post-handshake AEAD-wrapped "live activity" push. Carries a
    /// `LiveActivity` JSON (app + title + text + progress) — an ongoing,
    /// progress-bearing phone notification (ride ETA, navigation, delivery,
    /// timer) that updates in place and drives a persistent pill on the
    /// laptop's top bar. Routed separately; never touches the audio handoff.
    pub const LIVE_ACTIVITY: u8 = 0x35;
    /// A chunk of an app-icon PNG (sent once per app, reassembled + cached on
    /// the laptop so mirrored notifications show the real app logo). Payload:
    /// `[app_id_len u8][app_id][total u16 BE][idx u16 BE][png-chunk bytes]`.
    /// Routed separately; never touches the audio handoff.
    pub const ICON: u8 = 0x36;
    /// An incoming/ongoing phone-call event mirrored from the phone (caller
    /// name/number, phase: ringing/active/ended, call start time). Drives a
    /// continuity-style call banner (ringing → Accept/Decline) then an
    /// in-call pill (caller + live duration → Mute/End). Routed separately from
    /// AUDIO_OP/STATE/NOTIFICATION/LIVE_ACTIVITY; never touches the audio
    /// handoff FSM.
    pub const CALL: u8 = 0x37;
    /// A laptop→phone call-control command (accept/decline/end/mute/unmute/
    /// speaker/sms-reject) acting on the current call. The control-side
    /// counterpart of CALL; never touches the audio handoff.
    pub const CALL_CONTROL: u8 = 0x38;
    /// One chunk of the phone's contacts list (name + numbers) for the laptop
    /// companion's Contacts page. Chunked like ICON: `[total u16 BE][idx u16
    /// BE][json-chunk]`. Routed separately; never touches the audio handoff.
    pub const CONTACTS: u8 = 0x39;
    /// One chunk of the phone's recent call log (number, name, type, time,
    /// duration) for the laptop companion's Recents page. Chunked like CONTACTS:
    /// `[total u16 BE][idx u16 BE][json-chunk]`. Routed separately.
    pub const CALL_LOG: u8 = 0x3A;
    /// One chunk of the phone's recent SMS messages (address, body, type, time,
    /// thread) for the laptop companion's Messages page. Chunked like CALL_LOG:
    /// `[total u16 BE][idx u16 BE][json-chunk]`. Routed separately.
    pub const SMS: u8 = 0x3B;
    /// One chunk of a single conversation's message page, sent on demand when the
    /// laptop opens a thread and scrolls up (infinite scroll). Requested via a
    /// `load_thread` CALL_CONTROL command; same chunk format as `SMS` but the UI
    /// MERGES it into the thread instead of replacing the recent list. Mirrors
    /// Kotlin `FrameType.SMS_THREAD`.
    pub const SMS_THREAD: u8 = 0x3C;
    /// LAN-only bulk-sync negotiation (never rides BLE). sub=0x01: the laptop
    /// sends `{"<dataset>":"<sha256-hex of its cached JSON>"}`; the phone
    /// replies with the dataset's chunked frames (e.g. CONTACTS) only for
    /// hashes that DIFFER, then sub=0x02 (done) with
    /// `{"<dataset>":"sent"|"match"}`. Unchanged data costs zero bytes; big
    /// lists ride reliable TCP instead of a BLE notify burst. Mirrors Kotlin
    /// `FrameType.BULK_SYNC`.
    pub const BULK_SYNC: u8 = 0x3D;
    /// One chunk of a call-log HISTORY batch (bulk-sync watermark dataset,
    /// LAN-only): same wire shape as CALL_LOG but MERGED into the laptop's
    /// persistent history store instead of replacing the recent list.
    /// Mirrors Kotlin `FrameType.CALL_LOG_HISTORY`.
    pub const CALL_LOG_HISTORY: u8 = 0x3E;
    /// One chunk of the phone's FULL SMS id list (bulk-sync hash dataset,
    /// LAN-only): lets the laptop prune history entries the phone deleted.
    /// Mirrors Kotlin `FrameType.SMS_IDS`.
    pub const SMS_IDS: u8 = 0x3F;
    /// A clipboard-sync push — text copied on one device, mirrored to the
    /// other (universal-clipboard style). Carries a `ClipboardMirror`
    /// JSON (text + ts). Bidirectional over the same AUDIO_SIGNAL sealed
    /// stream as NOTIFICATION. Mirrors Kotlin `FrameType.CLIPBOARD`.
    pub const CLIPBOARD: u8 = 0x40;
    /// One chunk of a clipboard IMAGE (PNG), `[total][idx][data]` like SMS —
    /// reassembled into the full image. Sent when a copied image fits the
    /// BLE size cap; larger images take the LAN path (future). Bidirectional.
    /// Mirrors Kotlin `FrameType.CLIPBOARD_IMAGE`.
    pub const CLIPBOARD_IMAGE: u8 = 0x41;
    /// A small "image available" signal (phone→laptop): the phone shared /
    /// copied an image and stashed it; the laptop should PULL it over LAN
    /// (reliable TCP via bulk-sync) by its token. Carries `{token, bytes}`
    /// JSON. Mirrors Kotlin `FrameType.CLIPBOARD_IMAGE_OFFER`.
    pub const CLIPBOARD_IMAGE_OFFER: u8 = 0x42;
    /// One chunk of a LONG clipboard text (`[total][idx][utf8]` like SMS) —
    /// used when a copied text is too big for a single CLIPBOARD frame. The
    /// chunks reassemble into the full UTF-8 string (never split mid-char).
    /// Bidirectional over AUDIO_SIGNAL. Mirrors Kotlin `FrameType.CLIPBOARD_TEXT`.
    pub const CLIPBOARD_TEXT: u8 = 0x43;
    /// One chunk of an instant-share-style shared FILE (`[total][idx][data]`), pulled
    /// over LAN (reliable TCP) after a CLIPBOARD_IMAGE_OFFER that carries the
    /// file's name+mime. Reassembled and saved to the laptop's Downloads — NOT
    /// the clipboard. Mirrors Kotlin `FrameType.CLIPBOARD_FILE`.
    pub const CLIPBOARD_FILE: u8 = 0x45;
    /// "Wi-Fi Direct ready" signal (phone→laptop): the phone brought up a 5 GHz
    /// P2P group owner for a fast instant-share pull. Carries `{ssid, pass}` JSON;
    /// the laptop joins that network and pulls pending files over the direct
    /// link (~20 MB/s vs the router path), then restores its Wi-Fi. Mirrors
    /// Kotlin `FrameType.WIFI_DIRECT_OFFER`.
    pub const WIFI_DIRECT_OFFER: u8 = 0x46;
    /// Phone → laptop browsing HANDOFF (seamless-continuity style): the URL the user
    /// is on / wants to open on the laptop, `{url, title, app_id, open_now}` JSON.
    /// `open_now=true` (an explicit Share) opens it immediately; `false` (the
    /// accessibility live-read) shows a "continue from phone" pill the user
    /// clicks to open. An empty `url` clears the pill. Mirrors Kotlin
    /// `FrameType.HANDOFF`. (0x47/0x48 are reserved by the screen-mirror module.)
    pub const HANDOFF: u8 = 0x4C;
    /// Laptop → phone file PUSH (instant-share, reverse): `FILE_PUSH_OFFER` carries
    /// `{name, mime, bytes}` JSON, then `FILE_PUSH` chunks (`[total][idx][data]`)
    /// stream the bytes. Sent over the LAN session after bulk-sync; the phone
    /// saves the file to its Downloads. Mirror Kotlin `FrameType.FILE_PUSH*`.
    pub const FILE_PUSH_OFFER: u8 = 0x49;
    pub const FILE_PUSH: u8 = 0x4A;
    /// Phone → laptop reply to a `FILE_PUSH_OFFER`: the AEAD payload is a single
    /// byte (1 = accept, 0 = decline) the user chose on the receiving phone.
    /// The laptop streams `FILE_PUSH` chunks only on accept (consent-gated share).
    /// Mirrors Kotlin `FrameType.FILE_PUSH_DECISION`.
    pub const FILE_PUSH_DECISION: u8 = 0x4B;
    /// Notes/Todos bidirectional sync. The full item set (incl. tombstones) is
    /// serialised to JSON and sent as `[total][idx][data]` chunks. BOTH devices
    /// push on connect + after a local edit; each side LWW-merges (`updated_at`)
    /// and replies with its merged set only if it holds items the sender lacked
    /// — converges in ≤2 rounds. Mirrors Kotlin `FrameType.NOTES_SYNC`.
    pub const NOTES_SYNC: u8 = 0x4D;
    /// Transport-level fragment of ONE oversized sealed frame riding the BLE
    /// AUDIO_SIGNAL notify channel. Android SILENTLY TRUNCATES a notify longer
    /// than ATT_MTU−3 (observed live: 529–696-byte NOTIFICATION frames capped
    /// at 514 → undecodable at the laptop AND a burned send nonce — exactly
    /// why long email notifications never arrived). The sender now splits the
    /// fully-ENCODED sealed frame into FRAG envelopes, payload
    /// `[total u16 BE][idx u16 BE][slice]`; the receiver reassembles the inner
    /// bytes and processes them as if they had arrived as one notify. FRAG
    /// itself is NOT sealed — the inner frame already is (one nonce per
    /// logical frame). Mirrors Kotlin `FrameType.FRAG`.
    /// A listing of one folder on the phone: `[total][idx][json]` chunks over
    /// the LAN session, answering a `browse` key in the bulk-sync request.
    ///
    /// Browsing, not syncing. The laptop asks for a folder and gets its
    /// entries; nothing is copied until the user picks a file, and then it
    /// comes back over [`CLIPBOARD_FILE`] like any other pull. The phone's
    /// storage is the one that stays authoritative — mirroring it would put a
    /// second copy of everything on a machine that did not ask for it.
    /// Mirrors Kotlin `FrameType.PHONE_FILES`.
    pub const PHONE_FILES: u8 = 0x4F;

    pub const FRAG: u8 = 0x4E;
    /// Session-ownership handoff (design doc §D4). A device may TRUST many
    /// peers but is ACTIVE with exactly one; this frame is how the two sides
    /// agree which. `sub` carries the kind ([`sub::HANDOFF_RELEASE`] etc.) and
    /// the AEAD payload an optional UTF-8 successor name for the UI (peer-
    /// supplied, so sanitise before display).
    ///
    /// Additive by design: both sides log-and-ignore an unknown frame type, so
    /// a peer without this build is unaffected. Mirrors Kotlin
    /// `FrameType.PEER_HANDOFF`.
    pub const PEER_HANDOFF: u8 = 0x4F;
    pub const ERROR: u8 = 0x7F;
}

/// Sub-type constants for `TRANSPORT_KEEPALIVE`.
pub mod sub {
    pub const PING: u8 = 0x01;
    pub const PONG: u8 = 0x02;
    pub const ECHO_REQUEST: u8 = 0x01;
    pub const ECHO_RESPONSE: u8 = 0x02;
    /// `PEER_HANDOFF` kinds. Mirror Kotlin `FrameSub.HANDOFF_*`.
    ///
    /// RELEASE: "you are no longer my active peer" — sent by the side handing
    /// ownership over, so the receiver stops presenting itself as connected
    /// instead of discovering it on the next contact.
    pub const HANDOFF_RELEASE: u8 = 0x01;
    /// BUSY: refused, another peer is already active. Explicit so a rejected
    /// peer can back off; silence is indistinguishable from packet loss and
    /// invites a retry loop against the phone's single GATT link.
    pub const HANDOFF_BUSY: u8 = 0x02;
    /// CLAIM: request to become the active peer.
    pub const HANDOFF_CLAIM: u8 = 0x03;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub ty: u8,
    pub sub: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameDecodeError {
    Short(usize),
    LengthTooLarge(usize),
    LengthMismatch { declared: usize, actual: usize },
}

impl std::fmt::Display for FrameDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Short(n) => write!(f, "frame too short: {n} bytes"),
            Self::LengthTooLarge(n) => write!(f, "declared length {n} exceeds max"),
            Self::LengthMismatch { declared, actual } => {
                write!(f, "length mismatch: declared {declared}, actual {actual}")
            }
        }
    }
}

impl std::error::Error for FrameDecodeError {}

impl Frame {
    pub fn new(ty: u8, sub: u8, payload: Vec<u8>) -> Self {
        Self { ty, sub, payload }
    }

    pub fn echo_request(payload: Vec<u8>) -> Self {
        Self::new(ty::TRANSPORT_KEEPALIVE, sub::ECHO_REQUEST, payload)
    }

    pub fn echo_response(payload: Vec<u8>) -> Self {
        Self::new(ty::TRANSPORT_KEEPALIVE, sub::ECHO_RESPONSE, payload)
    }

    /// Encode header + payload into a contiguous byte vector.
    pub fn encode(&self) -> Vec<u8> {
        assert!(self.payload.len() <= MAX_FRAME_PAYLOAD);
        let mut out = Vec::with_capacity(FRAME_HEADER_LEN + self.payload.len());
        out.push(self.ty);
        out.push(self.sub);
        out.extend_from_slice(&(self.payload.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decode a complete frame from `bytes`. Trailing data is rejected.
    pub fn decode(bytes: &[u8]) -> Result<Self, FrameDecodeError> {
        if bytes.len() < FRAME_HEADER_LEN {
            return Err(FrameDecodeError::Short(bytes.len()));
        }
        let length = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
        if length > MAX_FRAME_PAYLOAD {
            return Err(FrameDecodeError::LengthTooLarge(length));
        }
        let total = FRAME_HEADER_LEN + length;
        if bytes.len() != total {
            return Err(FrameDecodeError::LengthMismatch {
                declared: total,
                actual: bytes.len(),
            });
        }
        Ok(Self {
            ty: bytes[0],
            sub: bytes[1],
            payload: bytes[FRAME_HEADER_LEN..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_payload_round_trip() {
        let f = Frame::new(0x11, 0x01, vec![]);
        let bytes = f.encode();
        assert_eq!(bytes, vec![0x11, 0x01, 0x00, 0x00]);
        assert_eq!(Frame::decode(&bytes).unwrap(), f);
    }

    #[test]
    fn small_payload_round_trip() {
        let f = Frame::echo_request(vec![0xAA, 0xBB, 0xCC]);
        let bytes = f.encode();
        assert_eq!(bytes, vec![0x30, 0x01, 0x00, 0x03, 0xAA, 0xBB, 0xCC]);
        assert_eq!(Frame::decode(&bytes).unwrap(), f);
    }

    #[test]
    fn rejects_short_header() {
        assert!(matches!(
            Frame::decode(&[0x10, 0x01]),
            Err(FrameDecodeError::Short(2))
        ));
    }

    #[test]
    fn rejects_length_mismatch() {
        // Header says length=5, actual payload is 3 bytes.
        let bytes = [0x10, 0x01, 0x00, 0x05, 0xAA, 0xBB, 0xCC];
        assert!(matches!(
            Frame::decode(&bytes),
            Err(FrameDecodeError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn rejects_oversize_length() {
        // length = 0x4000 + 1 (over MAX_FRAME_PAYLOAD = 8192).
        let mut bytes = vec![0x10, 0x01];
        bytes.extend_from_slice(&((MAX_FRAME_PAYLOAD as u16) + 1).to_be_bytes());
        assert!(matches!(
            Frame::decode(&bytes),
            Err(FrameDecodeError::LengthTooLarge(_))
        ));
    }

    #[test]
    fn type_constants_match_spec() {
        assert_eq!(ty::TRANSPORT_KEEPALIVE, 0x30);
        assert_eq!(ty::PAIRING_HANDSHAKE, 0x10);
        assert_eq!(ty::ERROR, 0x7F);
    }
}
