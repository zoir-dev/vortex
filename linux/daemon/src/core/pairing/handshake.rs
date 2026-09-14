//! Noise XX initiator driver per spec §6.4.
//!
//! Drives the three XX messages over Vortex Pairing Control frames. The
//! initiator is the **Linux** Device per spec §6.1.

use std::time::Duration;

use snow::{params::NoiseParams, Builder, HandshakeState};
use tokio::time::timeout;
use tracing::{debug, info};

use crate::core::ble::PAIRING_CONTROL_UUID;
use crate::core::platform::GattLink;
use crate::core::ble::frame::{ty, Frame, FrameDecodeError};
use crate::core::crypto::derive::derive_prs;
use crate::core::crypto::noise::{NOISE_XX, PROLOGUE_XX};
use crate::core::crypto::sas::derive_sas;
use crate::core::crypto::x25519::X25519SecBytes;

/// Outcome of a successful XX handshake.
#[derive(Debug, Clone)]
pub struct XxOutcome {
    /// Post-handshake transcript hash (`h`, 32 bytes).
    pub transcript_hash: Vec<u8>,
    /// Peer's static public key (32 bytes), recovered from msg2.
    pub peer_static_pub: [u8; 32],
    /// 6-digit SAS string per spec §6.5.1.
    pub sas_string: String,
    /// Internal SAS integer (0..1_000_000).
    pub sas_value: u32,
}

/// Local user's pairing decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalDecision {
    Approve,
    Reject,
}

/// Final pairing outcome after both sides exchange approval frames.
#[derive(Debug, Clone)]
pub struct PairingOutcome {
    pub xx: XxOutcome,
    /// Pairwise Reconnect Secret (32 bytes), only present on success.
    pub prs: [u8; 32],
    /// Friendly device name carried in the peer's APPROVE frame payload.
    /// `None` if the peer didn't send one (legacy responder).
    pub peer_name: Option<String>,
}

#[derive(Debug)]
pub enum HandshakeError {
    Snow(snow::Error),
    /// The GATT link failed the write or subscribe. A `String` because
    /// [`GattLink`] is the seam — BlueZ and WinRT share no error type, and
    /// callers only log it.
    Link(String),
    UnexpectedFrame { ty: u8, sub: u8 },
    FrameDecode(FrameDecodeError),
    Timeout(&'static str),
    NoPeerStatic,
    LocalRejected,
    PeerRejected,
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Snow(e) => write!(f, "noise: {e}"),
            Self::Link(e) => write!(f, "gatt link: {e}"),
            Self::UnexpectedFrame { ty, sub } => {
                write!(f, "unexpected frame type=0x{ty:02x} sub=0x{sub:02x}")
            }
            Self::FrameDecode(e) => write!(f, "frame decode: {e}"),
            Self::Timeout(what) => write!(f, "timeout: {what}"),
            Self::NoPeerStatic => write!(f, "noise XX did not yield peer static public key"),
            Self::LocalRejected => write!(f, "local user rejected the pairing"),
            Self::PeerRejected => write!(f, "peer rejected the pairing"),
        }
    }
}

impl std::error::Error for HandshakeError {}

impl From<snow::Error> for HandshakeError {
    fn from(e: snow::Error) -> Self {
        Self::Snow(e)
    }
}

impl From<String> for HandshakeError {
    fn from(e: String) -> Self {
        Self::Link(e)
    }
}

fn build_initiator(static_priv: &X25519SecBytes) -> Result<HandshakeState, snow::Error> {
    let params: NoiseParams = NOISE_XX.parse()?;
    Builder::new(params)
        .local_private_key(static_priv)?
        .prologue(PROLOGUE_XX)?
        .build_initiator()
}

/// One frame to Pairing Control, unacknowledged (§9.1): the flow is driven by
/// the notification each write provokes, so an ATT ack would add a round trip
/// and no reliability. `VortexClient::write_pairing_control` used to encode that
/// choice; now the `false` does.
async fn write_pairing(link: &dyn GattLink, frame: &Frame) -> Result<(), HandshakeError> {
    link.write(PAIRING_CONTROL_UUID.as_u128(), &frame.encode(), false)
        .await
        .map_err(HandshakeError::Link)
}

/// Run Noise XX against the peer on `link`, using the supplied static private
/// key. Production callers pass the long-lived identity scalar.
///
/// Bounded by `wait_per_step` for each notify step.
pub async fn run_xx_initiator(
    link: &dyn GattLink,
    static_priv: &X25519SecBytes,
    wait_per_step: Duration,
) -> Result<XxOutcome, HandshakeError> {
    let mut handshake = build_initiator(static_priv)?;
    let mut buffer = vec![0u8; 1024];
    let mut payload_scratch = vec![0u8; 1024];

    // Subscribe BEFORE writing msg1 so we don't miss the msg2 notification.
    let (tx, mut notifies) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    link.subscribe(PAIRING_CONTROL_UUID.as_u128(), tx).await?;

    // ---- msg1 (initiator → responder) ----
    let n = handshake.write_message(&[], &mut buffer)?;
    debug!(bytes = n, "noise xx msg1");
    let frame = Frame::new(ty::PAIRING_HANDSHAKE, 0x01, buffer[..n].to_vec());
    write_pairing(link, &frame).await?;
    info!("→ msg1 sent ({} bytes)", n);

    // ---- msg2 (responder → initiator) ----
    let raw = timeout(wait_per_step, notifies.recv())
        .await
        .map_err(|_| HandshakeError::Timeout("msg2 notify"))?
        .ok_or(HandshakeError::Timeout("notify stream closed"))?;
    let msg2 = Frame::decode(&raw).map_err(HandshakeError::FrameDecode)?;
    if msg2.ty != ty::PAIRING_HANDSHAKE || msg2.sub != 0x02 {
        return Err(HandshakeError::UnexpectedFrame {
            ty: msg2.ty,
            sub: msg2.sub,
        });
    }
    handshake.read_message(&msg2.payload, &mut payload_scratch)?;
    info!("← msg2 received ({} bytes)", msg2.payload.len());

    // ---- msg3 (initiator → responder) ----
    let n = handshake.write_message(&[], &mut buffer)?;
    debug!(bytes = n, "noise xx msg3");
    let frame = Frame::new(ty::PAIRING_HANDSHAKE, 0x03, buffer[..n].to_vec());
    write_pairing(link, &frame).await?;
    info!("→ msg3 sent ({} bytes)", n);

    // Snow advances to transport mode after the third XX message; harvest
    // outputs before consuming the state.
    let transcript_hash = handshake.get_handshake_hash().to_vec();
    let peer_static_pub_slice = handshake
        .get_remote_static()
        .ok_or(HandshakeError::NoPeerStatic)?;
    let mut peer_static_pub = [0u8; 32];
    peer_static_pub.copy_from_slice(peer_static_pub_slice);

    let (sas_value, sas_string) = derive_sas(&transcript_hash);

    Ok(XxOutcome {
        transcript_hash,
        peer_static_pub,
        sas_string,
        sas_value,
    })
}

/// Run XX, then exchange dual-approval frames. The local decision is
/// supplied by `decide` (which receives the SAS string and returns
/// Approve/Reject — typically prompts the user).
///
/// On both-sides-approve, returns the [`PairingOutcome`] including the
/// derived PRS. Otherwise returns [`HandshakeError::LocalRejected`] or
/// [`HandshakeError::PeerRejected`].
pub async fn run_pairing_initiator<F, Fut>(
    link: &dyn GattLink,
    static_priv: &X25519SecBytes,
    wait_per_step: Duration,
    decide: F,
    local_name: Option<&str>,
) -> Result<PairingOutcome, HandshakeError>
where
    F: FnOnce(&str) -> Fut,
    Fut: std::future::Future<Output = LocalDecision>,
{
    // Subscribe to PairingControl notifications BEFORE writing msg1 and keep
    // the channel alive across XX + approval — the subscription outlives every
    // step, so a notification can never land between two of them unheard.
    let (tx, mut notifies) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    link.subscribe(PAIRING_CONTROL_UUID.as_u128(), tx).await?;

    let mut handshake = build_initiator(static_priv)?;
    let mut buffer = vec![0u8; 1024];
    let mut payload_scratch = vec![0u8; 1024];

    // ---- XX msg1 ----
    let n = handshake.write_message(&[], &mut buffer)?;
    let frame = Frame::new(ty::PAIRING_HANDSHAKE, 0x01, buffer[..n].to_vec());
    write_pairing(link, &frame).await?;
    info!("→ msg1 sent ({} bytes)", n);

    // ---- XX msg2 ----
    let raw = timeout(wait_per_step, notifies.recv())
        .await
        .map_err(|_| HandshakeError::Timeout("msg2 notify"))?
        .ok_or(HandshakeError::Timeout("notify stream closed"))?;
    let msg2 = Frame::decode(&raw).map_err(HandshakeError::FrameDecode)?;
    if msg2.ty != ty::PAIRING_HANDSHAKE || msg2.sub != 0x02 {
        return Err(HandshakeError::UnexpectedFrame {
            ty: msg2.ty,
            sub: msg2.sub,
        });
    }
    handshake.read_message(&msg2.payload, &mut payload_scratch)?;
    info!("← msg2 received ({} bytes)", msg2.payload.len());

    // ---- XX msg3 ----
    let n = handshake.write_message(&[], &mut buffer)?;
    let frame = Frame::new(ty::PAIRING_HANDSHAKE, 0x03, buffer[..n].to_vec());
    write_pairing(link, &frame).await?;
    info!("→ msg3 sent ({} bytes)", n);

    // Harvest XX outputs.
    let transcript_hash = handshake.get_handshake_hash().to_vec();
    let peer_static_pub_slice = handshake
        .get_remote_static()
        .ok_or(HandshakeError::NoPeerStatic)?;
    let mut peer_static_pub = [0u8; 32];
    peer_static_pub.copy_from_slice(peer_static_pub_slice);

    // Promote to Noise transport mode. Every post-handshake frame
    // (APPROVE / REJECT and any V2 application data) MUST flow through
    // AEAD so that a network attacker can't tamper with the dual-
    // approval gate or with the embedded peer_name even if they
    // somehow racked the BLE / LAN socket.
    let mut transport = handshake.into_transport_mode()?;

    let (sas_value, sas_string) = derive_sas(&transcript_hash);
    let xx = XxOutcome {
        transcript_hash: transcript_hash.clone(),
        peer_static_pub,
        sas_string: sas_string.clone(),
        sas_value,
    };

    // ---- Dual approval ----
    // The decision callback is async: callers that gate on real user input
    // (Tauri Approve/Reject button) drive a channel here; pure CLI callers
    // can return an immediate `async { LocalDecision::Approve }`.
    let local = decide(&sas_string).await;
    let approval_sub = if local == LocalDecision::Approve { 0x01 } else { 0x02 };
    // Carry our friendly device name as the APPROVE-frame payload so
    // the peer can show "MyLaptop" instead of "Linux laptop". Plain
    // utf-8 — the name isn't a secret. Empty payload on reject. The
    // name is sanitized + length-capped so a misbehaving local hook
    // can't push control chars, bidi overrides, or large blobs into
    // the peer's trust store.
    let approval_plain = if local == LocalDecision::Approve {
        sanitize_peer_name(local_name.unwrap_or("")).into_bytes()
    } else {
        Vec::new()
    };
    // AEAD-wrap before the bytes hit the wire.
    let mut approval_ct = vec![0u8; approval_plain.len() + 16];
    let approval_ct_len = transport.write_message(&approval_plain, &mut approval_ct)?;
    approval_ct.truncate(approval_ct_len);
    write_pairing(
        link,
        &Frame::new(ty::PAIRING_APPROVAL, approval_sub, approval_ct),
    )
    .await?;
    info!(
        "→ approval sent ({} ct bytes): {}",
        approval_ct_len,
        if local == LocalDecision::Approve { "approve" } else { "reject" },
    );

    if local == LocalDecision::Reject {
        return Err(HandshakeError::LocalRejected);
    }

    // Wait for peer's approval frame.
    let raw = timeout(wait_per_step, notifies.recv())
        .await
        .map_err(|_| HandshakeError::Timeout("peer approval"))?
        .ok_or(HandshakeError::Timeout("notify stream closed"))?;
    let peer_decision = Frame::decode(&raw).map_err(HandshakeError::FrameDecode)?;
    if peer_decision.ty != ty::PAIRING_APPROVAL {
        return Err(HandshakeError::UnexpectedFrame {
            ty: peer_decision.ty,
            sub: peer_decision.sub,
        });
    }
    // AEAD-unwrap the peer's approval. A tampered frame fails here.
    let mut peer_pt = vec![0u8; peer_decision.payload.len()];
    let peer_pt_len = transport.read_message(&peer_decision.payload, &mut peer_pt)?;
    peer_pt.truncate(peer_pt_len);
    info!(
        "← peer approval ({} pt bytes): {}",
        peer_pt_len,
        if peer_decision.sub == 0x01 { "approve" } else { "reject" },
    );
    if peer_decision.sub != 0x01 {
        return Err(HandshakeError::PeerRejected);
    }

    // Extract peer's friendly name from their (now decrypted) APPROVE
    // payload. Defense-in-depth: sanitize whatever the peer sent —
    // post-XX they're authenticated but may still be buggy / hostile.
    let peer_name = std::str::from_utf8(&peer_pt)
        .ok()
        .map(sanitize_peer_name)
        .filter(|s| !s.is_empty());

    // Derive PRS only after both approves.
    let prs = derive_prs(&transcript_hash);
    Ok(PairingOutcome { xx, prs, peer_name })
}

/// Maximum number of Unicode scalar values we accept in a peer name.
/// Picked to fit a typical hostname / Build.MODEL plus a small margin.
const PEER_NAME_MAX_CHARS: usize = 64;

/// Sanitize a peer-supplied or locally-supplied device name before it
/// crosses the wire or lands in trust storage:
///   * strip C0/C1 control chars (NUL, BEL, CRLF, etc.);
///   * strip Unicode bidi-override chars (U+202A..U+202E, U+2066..U+2069)
///     which can disguise hostile strings as benign ones;
///   * cap to PEER_NAME_MAX_CHARS scalar values;
///   * trim surrounding whitespace.
///
/// Returns an empty string if everything was filtered out.
pub fn sanitize_peer_name(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .filter(|c| {
            // Drop C0 (< 0x20), DEL, and C1 (0x80..=0x9F) ranges.
            if (*c as u32) < 0x20 || (*c as u32) == 0x7F {
                return false;
            }
            if (0x80..=0x9F).contains(&(*c as u32)) {
                return false;
            }
            // Drop bidi-override / isolate marks.
            matches!(
                *c as u32,
                0x202A..=0x202E | 0x2066..=0x2069
            )
            .then(|| false)
            .unwrap_or(true)
        })
        .take(PEER_NAME_MAX_CHARS)
        .collect();
    cleaned.trim().to_string()
}

#[cfg(test)]
mod sanitize_tests {
    use super::sanitize_peer_name;

    #[test]
    fn passes_simple_name() {
        assert_eq!(sanitize_peer_name("zoyirjon-Blade"), "zoyirjon-Blade");
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(sanitize_peer_name("  My Phone  "), "My Phone");
    }

    #[test]
    fn rejects_control_chars() {
        let name = "evil\x00\x07\x1bhost";
        assert_eq!(sanitize_peer_name(name), "evilhost");
    }

    #[test]
    fn rejects_bidi_override() {
        // U+202E RIGHT-TO-LEFT OVERRIDE
        let name = "safe\u{202E}name";
        assert_eq!(sanitize_peer_name(name), "safename");
    }

    #[test]
    fn caps_length() {
        let name = "a".repeat(300);
        assert_eq!(sanitize_peer_name(&name).chars().count(), 64);
    }

    #[test]
    fn returns_empty_when_fully_filtered() {
        assert_eq!(sanitize_peer_name("\x00\x01\x02"), "");
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;
    use crate::core::crypto::noise::{NOISE_XX, PROLOGUE_XX};
    use crate::core::platform::FakeGattLink;
    use snow::Builder;

    fn uuid() -> crate::core::platform::Uuid128 {
        PAIRING_CONTROL_UUID.as_u128()
    }

    fn fake() -> FakeGattLink {
        FakeGattLink::new(vec![uuid()])
    }

    /// Wait until the initiator has written `n` frames, then return the last.
    ///
    /// Polling rather than a callback because `FakeGattLink` deliberately has
    /// no notion of "react to a write" — it is a recorder, and the test drives
    /// the peer side itself.
    async fn nth_write(fake: &FakeGattLink, n: usize) -> Frame {
        for _ in 0..200 {
            {
                let w = fake.writes.lock().unwrap();
                if w.len() >= n {
                    return Frame::decode(&w[n - 1].1).expect("well-formed frame");
                }
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("initiator never wrote frame {n}");
    }

    /// A full XX pairing with dual approval, both sides in one test and no
    /// radio anywhere: the test plays the phone.
    ///
    /// This is the flow that decides whether two devices trust each other
    /// forever, and until the seam existed it could only be exercised with a
    /// real phone, a real adapter and a human reading a code off a screen. The
    /// property that matters most is the one asserted last: BOTH sides derive
    /// the same SAS, because that is the only thing standing between the user
    /// and a man in the middle.
    #[tokio::test]
    async fn a_full_pairing_completes_and_both_sides_agree_on_the_sas() {
        let fake = fake();
        let responder_priv = [0x42u8; 32];

        let phone = async {
            let mut resp = Builder::new(NOISE_XX.parse().unwrap())
                .local_private_key(&responder_priv)
                .unwrap()
                .prologue(PROLOGUE_XX)
                .unwrap()
                .build_responder()
                .unwrap();
            let mut scratch = vec![0u8; 1024];
            let mut out = vec![0u8; 1024];

            // msg1 → msg2
            let msg1 = nth_write(&fake, 1).await;
            assert_eq!((msg1.ty, msg1.sub), (ty::PAIRING_HANDSHAKE, 0x01));
            resp.read_message(&msg1.payload, &mut scratch).unwrap();
            let n = resp.write_message(&[], &mut out).unwrap();
            fake.push_notification(
                uuid(),
                Frame::new(ty::PAIRING_HANDSHAKE, 0x02, out[..n].to_vec()).encode(),
            );

            // msg3 completes XX
            let msg3 = nth_write(&fake, 2).await;
            assert_eq!((msg3.ty, msg3.sub), (ty::PAIRING_HANDSHAKE, 0x03));
            resp.read_message(&msg3.payload, &mut scratch).unwrap();
            let responder_hash = resp.get_handshake_hash().to_vec();
            let mut transport = resp.into_transport_mode().unwrap();

            // The laptop's APPROVE arrives AEAD-wrapped; a tampered frame
            // would fail right here, which is the point of wrapping it.
            let approval = nth_write(&fake, 3).await;
            assert_eq!(approval.ty, ty::PAIRING_APPROVAL);
            assert_eq!(approval.sub, 0x01, "approve");
            let mut pt = vec![0u8; approval.payload.len()];
            let len = transport.read_message(&approval.payload, &mut pt).unwrap();
            assert_eq!(&pt[..len], b"test-laptop", "our name, decrypted");

            // Answer with our own approval.
            let mut ct = vec![0u8; 64];
            let n = transport.write_message(b"test-phone", &mut ct).unwrap();
            fake.push_notification(
                uuid(),
                Frame::new(ty::PAIRING_APPROVAL, 0x01, ct[..n].to_vec()).encode(),
            );
            responder_hash
        };

        let laptop = run_pairing_initiator(
            &fake,
            &[0x11u8; 32],
            Duration::from_secs(5),
            |_sas| async { LocalDecision::Approve },
            Some("test-laptop"),
        );

        let (outcome, responder_hash) = tokio::join!(laptop, phone);
        let outcome = outcome.expect("pairing should complete");

        assert_eq!(outcome.peer_name.as_deref(), Some("test-phone"));
        // The SAS the user compares is derived from the transcript, so both
        // sides MUST agree — a mismatch is exactly what a MITM produces.
        assert_eq!(outcome.xx.transcript_hash, responder_hash);
        let (_, phone_sas) = crate::core::crypto::sas::derive_sas(&responder_hash);
        assert_eq!(outcome.xx.sas_string, phone_sas);
        assert_eq!(outcome.xx.sas_string.len(), 6);
        // PRS comes from the transcript too, and only after both approved.
        assert_eq!(outcome.prs, crate::core::crypto::derive::derive_prs(&responder_hash));
    }

    /// A local reject must still TELL the peer, then fail — otherwise the phone
    /// sits waiting on a pairing the user already refused.
    #[tokio::test]
    async fn a_local_reject_sends_a_reject_frame_and_then_fails() {
        let fake = fake();
        let phone = async {
            let mut resp = Builder::new(NOISE_XX.parse().unwrap())
                .local_private_key(&[0x42u8; 32])
                .unwrap()
                .prologue(PROLOGUE_XX)
                .unwrap()
                .build_responder()
                .unwrap();
            let mut scratch = vec![0u8; 1024];
            let mut out = vec![0u8; 1024];
            let msg1 = nth_write(&fake, 1).await;
            resp.read_message(&msg1.payload, &mut scratch).unwrap();
            let n = resp.write_message(&[], &mut out).unwrap();
            fake.push_notification(
                uuid(),
                Frame::new(ty::PAIRING_HANDSHAKE, 0x02, out[..n].to_vec()).encode(),
            );
            nth_write(&fake, 3).await
        };

        let laptop = run_pairing_initiator(
            &fake,
            &[0x11u8; 32],
            Duration::from_secs(5),
            |_sas| async { LocalDecision::Reject },
            Some("test-laptop"),
        );

        let (outcome, reject_frame) = tokio::join!(laptop, phone);
        assert!(matches!(outcome, Err(HandshakeError::LocalRejected)));
        assert_eq!(reject_frame.ty, ty::PAIRING_APPROVAL);
        assert_eq!(reject_frame.sub, 0x02, "reject");
        // No name leaks to a peer we just refused.
        assert!(reject_frame.payload.len() <= 16, "empty plaintext + AEAD tag");
    }
}
