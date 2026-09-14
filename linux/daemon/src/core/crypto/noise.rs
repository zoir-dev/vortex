//! Noise XX and Noise IK runners per spec §6 and §7.
//!
//! Patterns and prologues are normative.

use snow::{params::NoiseParams, Builder};

/// Re-exported so a consumer can NAME the state a completed handshake hands
/// back without taking its own `snow` dependency. Two copies of snow in one
/// build are two incompatible `TransportState`s, and the mismatch surfaces as a
/// baffling type error at an API boundary rather than as a version conflict.
pub use snow::TransportState;

pub const NOISE_XX: &str = "Noise_XX_25519_ChaChaPoly_SHA256";
/// The reconnect pattern — used by `run_ik_deterministic` for the test
/// vector AND at runtime. Runtime additionally mixes the Pairwise Reconnect
/// Secret into the prologue (see [`prologue_with_prs`]), which is what
/// keeps a reconnect authenticated after a long-term static-key compromise.
/// That is the goal `Noise_IKpsk2` would serve; the prologue route reaches it
/// without a pattern the Android-side Noise library does not implement.
pub const NOISE_IK: &str = "Noise_IK_25519_ChaChaPoly_SHA256";

pub const PROLOGUE_XX: &[u8] = b"vortex/v1/pairing";
pub const PROLOGUE_IK: &[u8] = b"vortex/v1/reconnect";

/// Build the IK prologue with the Pairwise Reconnect Secret mixed in.
///
/// We extend the base prologue with the 32-byte PRS so that any wrong-PRS
/// attempt by an attacker who has compromised only the long-term static
/// private key fails AEAD verification on msg1's `s` decryption. This achieves
/// the same security goal as Noise_IKpsk2_... — binding reconnect to BOTH
/// static keys AND the prior pairing transcript — without requiring a Noise
/// pattern that the Android-side library does not yet implement.
///
/// Lives HERE, with the prologue it extends, rather than in the BLE reconnect
/// module it was first written in: both transports need it (`lan::tcp_client`
/// runs the same IK over TCP) and it is normative wire material, so it must
/// not sit behind a platform gate.
pub(crate) fn prologue_with_prs(prs: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PROLOGUE_IK.len() + 32);
    out.extend_from_slice(PROLOGUE_IK);
    out.extend_from_slice(prs);
    out
}

/// Result of a deterministic handshake run.
#[derive(Debug, Clone)]
pub struct HandshakeResult {
    /// Wire bytes for each Noise message in order.
    /// XX produces 3 messages; IK produces 2.
    pub messages: Vec<Vec<u8>>,
    /// Post-handshake transcript hash (`h`) as observed by the initiator.
    pub initiator_handshake_hash: Vec<u8>,
    /// Post-handshake transcript hash as observed by the responder.
    /// MUST equal `initiator_handshake_hash` on success.
    pub responder_handshake_hash: Vec<u8>,
}

/// Run Noise XX with deterministic ephemeral keys (test-only).
pub fn run_xx_deterministic(
    initiator_static_priv: &[u8],
    responder_static_priv: &[u8],
    initiator_ephemeral_priv: &[u8],
    responder_ephemeral_priv: &[u8],
) -> Result<HandshakeResult, snow::Error> {
    let params: NoiseParams = NOISE_XX.parse()?;
    let mut initiator = Builder::new(params.clone())
        .local_private_key(initiator_static_priv)?
        .fixed_ephemeral_key_for_testing_only(initiator_ephemeral_priv)
        .prologue(PROLOGUE_XX)?
        .build_initiator()?;
    let mut responder = Builder::new(params)
        .local_private_key(responder_static_priv)?
        .fixed_ephemeral_key_for_testing_only(responder_ephemeral_priv)
        .prologue(PROLOGUE_XX)?
        .build_responder()?;

    let mut buffer = vec![0u8; 1024];
    let mut tmp = vec![0u8; 1024];
    let mut messages = Vec::with_capacity(3);

    let len = initiator.write_message(&[], &mut buffer)?;
    messages.push(buffer[..len].to_vec());
    responder.read_message(&messages[0], &mut tmp)?;

    let len = responder.write_message(&[], &mut buffer)?;
    messages.push(buffer[..len].to_vec());
    initiator.read_message(&messages[1], &mut tmp)?;

    let len = initiator.write_message(&[], &mut buffer)?;
    messages.push(buffer[..len].to_vec());
    responder.read_message(&messages[2], &mut tmp)?;

    Ok(HandshakeResult {
        messages,
        initiator_handshake_hash: initiator.get_handshake_hash().to_vec(),
        responder_handshake_hash: responder.get_handshake_hash().to_vec(),
    })
}

/// Run Noise IK with deterministic ephemeral keys (test-only).
pub fn run_ik_deterministic(
    initiator_static_priv: &[u8],
    responder_static_priv: &[u8],
    initiator_ephemeral_priv: &[u8],
    responder_ephemeral_priv: &[u8],
    responder_static_pub: &[u8],
) -> Result<HandshakeResult, snow::Error> {
    let params: NoiseParams = NOISE_IK.parse()?;
    let mut initiator = Builder::new(params.clone())
        .local_private_key(initiator_static_priv)?
        .remote_public_key(responder_static_pub)?
        .fixed_ephemeral_key_for_testing_only(initiator_ephemeral_priv)
        .prologue(PROLOGUE_IK)?
        .build_initiator()?;
    let mut responder = Builder::new(params)
        .local_private_key(responder_static_priv)?
        .fixed_ephemeral_key_for_testing_only(responder_ephemeral_priv)
        .prologue(PROLOGUE_IK)?
        .build_responder()?;

    let mut buffer = vec![0u8; 1024];
    let mut tmp = vec![0u8; 1024];
    let mut messages = Vec::with_capacity(2);

    let len = initiator.write_message(&[], &mut buffer)?;
    messages.push(buffer[..len].to_vec());
    responder.read_message(&messages[0], &mut tmp)?;

    let len = responder.write_message(&[], &mut buffer)?;
    messages.push(buffer[..len].to_vec());
    initiator.read_message(&messages[1], &mut tmp)?;

    Ok(HandshakeResult {
        messages,
        initiator_handshake_hash: initiator.get_handshake_hash().to_vec(),
        responder_handshake_hash: responder.get_handshake_hash().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed test seeds (NOT real keys; for deterministic vectors only).
    const INIT_S: &[u8; 32] = b"\x10\x11\x12\x13\x14\x15\x16\x17\x18\x19\x1a\x1b\x1c\x1d\x1e\x1f\x20\x21\x22\x23\x24\x25\x26\x27\x28\x29\x2a\x2b\x2c\x2d\x2e\x2f";
    const RESP_S: &[u8; 32] = b"\x30\x31\x32\x33\x34\x35\x36\x37\x38\x39\x3a\x3b\x3c\x3d\x3e\x3f\x40\x41\x42\x43\x44\x45\x46\x47\x48\x49\x4a\x4b\x4c\x4d\x4e\x4f";
    const INIT_E: &[u8; 32] = b"\x50\x51\x52\x53\x54\x55\x56\x57\x58\x59\x5a\x5b\x5c\x5d\x5e\x5f\x60\x61\x62\x63\x64\x65\x66\x67\x68\x69\x6a\x6b\x6c\x6d\x6e\x6f";
    const RESP_E: &[u8; 32] = b"\x70\x71\x72\x73\x74\x75\x76\x77\x78\x79\x7a\x7b\x7c\x7d\x7e\x7f\x80\x81\x82\x83\x84\x85\x86\x87\x88\x89\x8a\x8b\x8c\x8d\x8e\x8f";

    fn x25519_pub(priv_bytes: &[u8; 32]) -> [u8; 32] {
        let sk = x25519_dalek::StaticSecret::from(*priv_bytes);
        x25519_dalek::PublicKey::from(&sk).to_bytes()
    }

    #[test]
    fn xx_byte_counts_match_spec() {
        let r = run_xx_deterministic(INIT_S, RESP_S, INIT_E, RESP_E).unwrap();
        // spec §6.4: msg1 = 32 bytes, msg2 = 96 bytes (no payload), msg3 = 64 bytes.
        // With empty payload: msg2 = 32 + 48 = 80? Let's just record actual sizes
        // and the spec's normative byte tables apply only when in-handshake
        // payload of 17 bytes is included. Empty payload changes counts.
        // For empty payload:
        //   msg1: 32 bytes (e only)
        //   msg2: 32 + 32 + 16 + 16 = 96 bytes (e, encrypted_s, payload_tag)
        //   msg3: 32 + 16 + 16 = 64 bytes (encrypted_s, payload_tag)
        assert_eq!(r.messages[0].len(), 32, "XX msg1 = e (32 bytes)");
        assert_eq!(
            r.messages[1].len(),
            96,
            "XX msg2 = e + s_ct + s_tag + payload_tag (no payload) = 96"
        );
        assert_eq!(
            r.messages[2].len(),
            64,
            "XX msg3 = s_ct + s_tag + payload_tag (no payload) = 64"
        );
        assert_eq!(
            r.initiator_handshake_hash, r.responder_handshake_hash,
            "transcript hashes must match"
        );
        assert_eq!(r.initiator_handshake_hash.len(), 32);
    }

    #[test]
    fn ik_byte_counts_and_transcript() {
        let resp_pub = x25519_pub(RESP_S);
        let r = run_ik_deterministic(INIT_S, RESP_S, INIT_E, RESP_E, &resp_pub).unwrap();
        // For empty payload:
        //   msg1: 32 + 32 + 16 + 16 = 96 bytes (e, encrypted_s, payload_tag)
        //   msg2: 32 + 16 = 48 bytes (e, payload_tag)
        assert_eq!(r.messages[0].len(), 96);
        assert_eq!(r.messages[1].len(), 48);
        assert_eq!(r.initiator_handshake_hash, r.responder_handshake_hash);
    }

    #[test]
    fn xx_is_deterministic() {
        let a = run_xx_deterministic(INIT_S, RESP_S, INIT_E, RESP_E).unwrap();
        let b = run_xx_deterministic(INIT_S, RESP_S, INIT_E, RESP_E).unwrap();
        assert_eq!(a.messages, b.messages);
        assert_eq!(a.initiator_handshake_hash, b.initiator_handshake_hash);
    }

    #[test]
    fn wrong_prologue_fails_responder() {
        // Run XX but force the responder to use a different prologue by
        // constructing it by hand. This verifies prologue is mixed in.
        let params: NoiseParams = NOISE_XX.parse().unwrap();
        let mut initiator = Builder::new(params.clone())
            .local_private_key(INIT_S)
            .unwrap()
            .fixed_ephemeral_key_for_testing_only(INIT_E)
            .prologue(PROLOGUE_XX)
            .unwrap()
            .build_initiator()
            .unwrap();
        let mut responder = Builder::new(params)
            .local_private_key(RESP_S)
            .unwrap()
            .fixed_ephemeral_key_for_testing_only(RESP_E)
            .prologue(b"different-prologue")
            .unwrap()
            .build_responder()
            .unwrap();

        let mut buf = vec![0u8; 1024];
        let mut tmp = vec![0u8; 1024];

        let len = initiator.write_message(&[], &mut buf).unwrap();
        responder.read_message(&buf[..len], &mut tmp).unwrap();
        // First message succeeds (no AEAD); diverging prologue is detected
        // when the next encrypted step fails.
        let len = responder.write_message(&[], &mut buf).unwrap();
        let result = initiator.read_message(&buf[..len], &mut tmp);
        assert!(result.is_err(), "mismatched prologue must fail AEAD");
    }

    /// The reconnect prologue is normative wire material: the phone builds the
    /// same bytes, and a mismatch fails AEAD on msg1 rather than producing a
    /// readable error. Pin the layout — base prologue, then the raw 32-byte
    /// PRS, nothing else — so a refactor can't silently reorder or pad it.
    #[test]
    fn ik_prologue_is_base_then_prs() {
        let prs = [0xAB; 32];
        let p = prologue_with_prs(&prs);
        assert_eq!(p.len(), PROLOGUE_IK.len() + 32);
        assert_eq!(&p[..PROLOGUE_IK.len()], PROLOGUE_IK);
        assert_eq!(&p[PROLOGUE_IK.len()..], &prs[..]);
        assert_eq!(&p[..PROLOGUE_IK.len()], b"vortex/v1/reconnect");
    }

    /// A different PRS must give a different prologue — that difference IS the
    /// binding to the prior pairing transcript.
    #[test]
    fn a_different_prs_gives_a_different_prologue() {
        assert_ne!(prologue_with_prs(&[1u8; 32]), prologue_with_prs(&[2u8; 32]));
    }
}
