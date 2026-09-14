//! Noise IK initiator + liveness probe per spec §7.

use std::time::Duration;

use rand::RngCore;
use snow::{params::NoiseParams, Builder, HandshakeState, TransportState};
use tokio::time::timeout;
use tracing::info;

use crate::core::ble::frame::{ty, Frame, FrameDecodeError};
use crate::core::ble::RECONNECT_CONTROL_UUID;
use crate::core::platform::GattLink;
use crate::core::crypto::noise::NOISE_IK;
use crate::core::crypto::x25519::X25519SecBytes;

#[derive(Debug)]
pub struct ReconnectOutcome {
    pub transcript_hash: Vec<u8>,
    pub peer_static_pub: [u8; 32],
    pub liveness_ok: bool,
    /// M6: counter value the peer reported in IK msg2 payload. Caller
    /// compares with their stored local counter — a `peer_counter`
    /// strictly less than `local_counter` is a backup-restore replay
    /// signal worth logging.
    pub peer_counter: u64,
    /// Noise transport-mode cipher pair derived from the IK handshake.
    /// Available so a caller can run an AEAD-protected post-handshake
    /// channel without re-running another IK (P2.13 BLE audio-signal
    /// path). `None` only if the caller used the legacy entrypoint that
    /// didn't ask for it — the standard path is always Some.
    pub transport: Option<TransportState>,
}

#[derive(Debug)]
pub enum ReconnectError {
    Snow(snow::Error),
    /// The GATT link failed the read, write or subscribe. A `String` because
    /// [`GattLink`] is the seam: BlueZ and WinRT have nothing in common to
    /// name here, and every caller only logs it.
    Link(String),
    Timeout(&'static str),
    UnexpectedFrame { ty: u8, sub: u8 },
    FrameDecode(FrameDecodeError),
    PeerMismatch,
    NoPeerStatic,
    LivenessNonceMismatch,
}

impl std::fmt::Display for ReconnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Snow(e) => write!(f, "noise: {e}"),
            Self::Link(e) => write!(f, "gatt link: {e}"),
            Self::Timeout(what) => write!(f, "timeout: {what}"),
            Self::UnexpectedFrame { ty, sub } => {
                write!(f, "unexpected frame type=0x{ty:02x} sub=0x{sub:02x}")
            }
            Self::FrameDecode(e) => write!(f, "frame decode: {e}"),
            Self::PeerMismatch => write!(f, "peer static public key did not match trusted record"),
            Self::NoPeerStatic => write!(f, "noise IK did not yield peer static public key"),
            Self::LivenessNonceMismatch => write!(f, "ping/pong nonce did not echo back"),
        }
    }
}

impl std::error::Error for ReconnectError {}

impl From<snow::Error> for ReconnectError {
    fn from(e: snow::Error) -> Self {
        Self::Snow(e)
    }
}

impl From<String> for ReconnectError {
    fn from(e: String) -> Self {
        Self::Link(e)
    }
}

/// One frame to Reconnect Control, unacknowledged (§9.1).
async fn write_reconnect(link: &dyn GattLink, frame: &Frame) -> Result<(), ReconnectError> {
    link.write(RECONNECT_CONTROL_UUID.as_u128(), &frame.encode(), false)
        .await
        .map_err(ReconnectError::Link)
}

fn build_ik_initiator(
    static_priv: &X25519SecBytes,
    peer_static_pub: &[u8; 32],
    prs: &[u8; 32],
) -> Result<HandshakeState, snow::Error> {
    let params: NoiseParams = NOISE_IK.parse()?;
    Builder::new(params)
        .local_private_key(static_priv)?
        .remote_public_key(peer_static_pub)?
        .prologue(&crate::core::crypto::noise::prologue_with_prs(prs))?
        .build_initiator()
}


/// Run Noise IK against the peer on `link`, using the local static identity,
/// the trusted peer's static public key, and the Pairwise Reconnect
/// Secret (mixed into the handshake prologue).
///
/// Binding the PRS via the prologue means a long-term static-key
/// compromise alone is not enough to impersonate the trusted peer —
/// the attacker would also need the PRS, which lives only in each
/// side's secure storage.
///
/// On success, the initiator follows up with a ping/pong liveness probe
/// (frame `0x30/0x01` → `0x30/0x02`) before returning.
pub async fn run_ik_initiator(
    link: &dyn GattLink,
    static_priv: &X25519SecBytes,
    peer_static_pub: &[u8; 32],
    prs: &[u8; 32],
    local_counter: u64,
    wait_per_step: Duration,
) -> Result<ReconnectOutcome, ReconnectError> {
    // Subscribe to Reconnect Control notifications BEFORE sending msg1: the
    // phone answers the moment it sees the write, and a notification that
    // arrives before we are listening is simply gone.
    let (tx, mut notifies) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    link.subscribe(RECONNECT_CONTROL_UUID.as_u128(), tx).await?;

    let mut handshake = build_ik_initiator(static_priv, peer_static_pub, prs)?;
    let mut buffer = vec![0u8; 1024];
    let mut payload_scratch = vec![0u8; 1024];

    // ---- IK msg1 ----
    // Payload carries the local reconnect counter (M6). Encrypted by
    // Noise IK from `es` onward, so a passive observer cannot read it.
    let counter_bytes = local_counter.to_be_bytes();
    let n = handshake.write_message(&counter_bytes, &mut buffer)?;
    let frame = Frame::new(ty::RECONNECT_HANDSHAKE, 0x01, buffer[..n].to_vec());
    // Write WITHOUT response throughout, per §9.1: the flow is driven by the
    // notification each write provokes, so an ATT ack adds a round trip and no
    // reliability. `write_reconnect_control` used to encode that choice; now
    // the `false` does.
    write_reconnect(link, &frame).await?;
    info!("→ IK msg1 sent ({} bytes, counter={local_counter})", n);

    // ---- IK msg2 ----
    let raw = timeout(wait_per_step, notifies.recv())
        .await
        .map_err(|_| ReconnectError::Timeout("msg2 notify"))?
        .ok_or(ReconnectError::Timeout("notify stream closed"))?;
    let msg2 = Frame::decode(&raw).map_err(ReconnectError::FrameDecode)?;
    if msg2.ty != ty::RECONNECT_HANDSHAKE || msg2.sub != 0x02 {
        return Err(ReconnectError::UnexpectedFrame {
            ty: msg2.ty,
            sub: msg2.sub,
        });
    }
    let pt_len = handshake.read_message(&msg2.payload, &mut payload_scratch)?;
    let peer_counter: u64 = if pt_len >= 8 {
        u64::from_be_bytes(payload_scratch[..8].try_into().unwrap())
    } else {
        0
    };
    info!(
        "← IK msg2 received ({} bytes, peer_counter={peer_counter})",
        msg2.payload.len()
    );

    // Verify peer's static matches the trusted record.
    let peer_pub_observed = handshake
        .get_remote_static()
        .ok_or(ReconnectError::NoPeerStatic)?;
    if peer_pub_observed != peer_static_pub {
        return Err(ReconnectError::PeerMismatch);
    }
    let transcript_hash = handshake.get_handshake_hash().to_vec();
    // Promote the IK handshake to transport mode BEFORE doing the
    // liveness probe — the ping/pong is plain ATT, but capturing the
    // ciphers here means we don't need a second IK over BLE for the
    // P2.13 audio-signal channel. The handshake is consumed so no
    // separate `drop(handshake)` is needed.
    let transport = handshake.into_transport_mode()?;

    // ---- Liveness probe (ping → pong) ----
    let mut nonce = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let ping = Frame::new(ty::TRANSPORT_KEEPALIVE, 0x01, nonce.to_vec());
    write_reconnect(link, &ping).await?;
    info!("→ ping ({})", hex::encode(nonce));

    let raw = timeout(wait_per_step, notifies.recv())
        .await
        .map_err(|_| ReconnectError::Timeout("pong"))?
        .ok_or(ReconnectError::Timeout("notify stream closed"))?;
    let pong = Frame::decode(&raw).map_err(ReconnectError::FrameDecode)?;
    if pong.ty != ty::TRANSPORT_KEEPALIVE || pong.sub != 0x02 {
        return Err(ReconnectError::UnexpectedFrame {
            ty: pong.ty,
            sub: pong.sub,
        });
    }
    if pong.payload.as_slice() != nonce {
        return Err(ReconnectError::LivenessNonceMismatch);
    }
    info!("← pong matched");

    Ok(ReconnectOutcome {
        transcript_hash,
        peer_static_pub: *peer_static_pub,
        liveness_ok: true,
        peer_counter,
        transport: Some(transport),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::platform::FakeGattLink;

    fn link() -> FakeGattLink {
        FakeGattLink::new(vec![RECONNECT_CONTROL_UUID.as_u128()])
    }

    /// What IK msg1 must look like on the wire, without a phone in the room.
    ///
    /// This is the first test of this flow that has ever been possible: before
    /// the seam it needed a BlueZ adapter and a real peer, so the frame type,
    /// the characteristic and the write mode were only ever verified by the
    /// handshake working end to end.
    #[tokio::test]
    async fn msg1_goes_out_unacknowledged_on_reconnect_control() {
        let fake = link();
        let err = run_ik_initiator(
            &fake,
            &[7u8; 32],
            &[9u8; 32],
            &[3u8; 32],
            42,
            Duration::from_millis(20),
        )
        .await
        .expect_err("no peer answers, so this must time out");
        assert!(matches!(err, ReconnectError::Timeout("msg2 notify")), "{err}");

        let writes = fake.writes.lock().unwrap();
        assert_eq!(writes.len(), 1, "exactly msg1, nothing speculative");
        let (uuid, bytes, with_response) = &writes[0];
        assert_eq!(*uuid, RECONNECT_CONTROL_UUID.as_u128());
        assert!(!with_response, "§9.1: unacknowledged writes");

        let frame = Frame::decode(bytes).expect("a well-formed frame");
        assert_eq!(frame.ty, ty::RECONNECT_HANDSHAKE);
        assert_eq!(frame.sub, 0x01);
        // Noise IK msg1 is e (32) + encrypted s (32+16) + encrypted payload
        // (8-byte counter + 16 tag): a fixed 104 bytes for our pattern. A
        // change here means the wire format moved.
        assert_eq!(frame.payload.len(), 104);
    }

    /// A frame that isn't msg2 must be rejected by type, not misparsed. The
    /// peer is unauthenticated at this point, so this is the boundary where a
    /// stray or hostile notification gets turned away.
    #[tokio::test]
    async fn a_wrong_frame_type_is_rejected_rather_than_decrypted() {
        let fake = link();
        let uuid = RECONNECT_CONTROL_UUID.as_u128();
        let driver = async {
            // Give the initiator a moment to subscribe and send msg1.
            tokio::time::sleep(Duration::from_millis(5)).await;
            fake.push_notification(uuid, Frame::new(ty::PAIRING_HANDSHAKE, 0x02, vec![0; 48]).encode());
        };
        let run = run_ik_initiator(
            &fake,
            &[7u8; 32],
            &[9u8; 32],
            &[3u8; 32],
            0,
            Duration::from_millis(200),
        );
        let (outcome, ()) = tokio::join!(run, driver);
        match outcome.expect_err("must not accept a foreign frame") {
            ReconnectError::UnexpectedFrame { ty, sub } => {
                assert_eq!((ty, sub), (crate::core::ble::frame::ty::PAIRING_HANDSHAKE, 0x02));
            }
            other => panic!("expected UnexpectedFrame, got {other}"),
        }
    }

    /// A link that can't carry the write fails the handshake with the reason,
    /// rather than hanging until the step timeout.
    #[tokio::test]
    async fn a_dead_link_fails_fast_with_its_own_error() {
        // Nothing present → subscribe itself fails.
        let fake = FakeGattLink::new(vec![]);
        let err = run_ik_initiator(
            &fake,
            &[7u8; 32],
            &[9u8; 32],
            &[3u8; 32],
            0,
            Duration::from_secs(30),
        )
        .await
        .expect_err("a link with no characteristic cannot handshake");
        assert!(matches!(err, ReconnectError::Link(_)), "{err}");
    }
}
