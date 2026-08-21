//! Noise IK initiator + liveness probe over TCP per spec §8.3 and §9.2.

use std::net::SocketAddr;
use std::time::Duration;

use rand::RngCore;
use snow::{params::NoiseParams, Builder, HandshakeState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::info;

use crate::core::ble::frame::{ty, Frame, FrameDecodeError, FRAME_HEADER_LEN, MAX_FRAME_PAYLOAD};
use crate::core::crypto::noise::NOISE_IK;
use crate::core::crypto::x25519::X25519SecBytes;

#[derive(Debug, Clone)]
pub struct LanReconnectOutcome {
    pub transcript_hash: Vec<u8>,
    pub peer_static_pub: [u8; 32],
    pub liveness_ok: bool,
    pub remote: SocketAddr,
    /// M6: peer's reported counter (see `ReconnectOutcome`).
    pub peer_counter: u64,
    /// Peer-reported app state (battery, device class, locale, theme,
    /// earbuds). Populated after the post-handshake app-data exchange.
    pub peer_state: Option<crate::core::appstate::AppState>,
    /// Bulk-sync datasets the phone shipped because our cached hash was
    /// stale: (frame type, reassembled JSON bytes). Empty when everything
    /// matched, no request was made, or the peer predates BULK_SYNC.
    pub bulk: Vec<(u8, Vec<u8>)>,
    /// Per-dataset outcome from the bulk-sync DONE frame, e.g.
    /// `{"contacts":"match","clipboard_file":"nomatch"}`. `None` when no
    /// request was made or the peer never sent a done frame (predates
    /// BULK_SYNC, or the exchange broke off).
    ///
    /// Datasets report their own failures HERE and nowhere else: a "nomatch"
    /// looks exactly like "nothing to send" in [`bulk`], so a caller holding a
    /// pull request open (the instant-share file queue) needs this to learn
    /// that what it asked for is never coming.
    pub bulk_status: Option<BulkStatus>,
}

/// The bulk-sync done frame's per-dataset outcome map.
#[derive(Debug, Clone, Default)]
pub struct BulkStatus(std::collections::HashMap<String, String>);

impl BulkStatus {
    /// Parse the done frame's JSON body. Non-string values are ignored rather
    /// than failing the whole map — one odd field must not blind the caller to
    /// the rest.
    fn parse(json: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(json).ok()?;
        let obj = v.as_object()?;
        Some(Self(
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect(),
        ))
    }

    /// What the phone reported for `dataset` ("sent" / "match" / "nomatch" /
    /// "error" / "unknown"), or `None` if it said nothing about it.
    pub fn get(&self, dataset: &str) -> Option<&str> {
        self.0.get(dataset).map(String::as_str)
    }

    /// True when the phone answered about `dataset` and it was NOT served —
    /// i.e. asking again is pointless until something changes on its side.
    pub fn unservable(&self, dataset: &str) -> bool {
        matches!(self.get(dataset), Some(s) if s != "sent")
    }
}

#[derive(Debug)]
pub enum LanError {
    Snow(snow::Error),
    Io(std::io::Error),
    Timeout(&'static str),
    UnexpectedFrame { ty: u8, sub: u8 },
    FrameDecode(FrameDecodeError),
    PeerMismatch,
    NoPeerStatic,
    LivenessNonceMismatch,
    OversizeFrame(usize),
}

impl std::fmt::Display for LanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Snow(e) => write!(f, "noise: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Timeout(what) => write!(f, "timeout: {what}"),
            Self::UnexpectedFrame { ty, sub } => {
                write!(f, "unexpected frame type=0x{ty:02x} sub=0x{sub:02x}")
            }
            Self::FrameDecode(e) => write!(f, "frame decode: {e}"),
            Self::PeerMismatch => write!(f, "peer static did not match trusted record"),
            Self::NoPeerStatic => write!(f, "noise IK did not yield peer static public key"),
            Self::LivenessNonceMismatch => write!(f, "ping/pong nonce did not echo back"),
            Self::OversizeFrame(n) => write!(f, "incoming frame declared length {n} > max"),
        }
    }
}

impl std::error::Error for LanError {}

impl From<snow::Error> for LanError {
    fn from(e: snow::Error) -> Self {
        Self::Snow(e)
    }
}
impl From<std::io::Error> for LanError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

pub(crate) fn build_ik_initiator(
    static_priv: &X25519SecBytes,
    peer_static_pub: &[u8; 32],
    prs: &[u8; 32],
) -> Result<HandshakeState, snow::Error> {
    // PRS is mixed into the prologue rather than as a Noise PSK,
    // because the Noise-Java library on Android does not implement the
    // standardized `psk<n>` modifier syntax. Mixing into the prologue
    // achieves the same goal — wrong PRS yields different MixHash and
    // breaks AEAD verification on msg1.
    let params: NoiseParams = NOISE_IK.parse()?;
    let prologue = crate::core::pairing::reconnect::prologue_with_prs(prs);
    Builder::new(params)
        .local_private_key(static_priv)?
        .remote_public_key(peer_static_pub)?
        .prologue(&prologue)?
        .build_initiator()
}

async fn write_frame(stream: &mut TcpStream, frame: &Frame) -> Result<(), LanError> {
    let bytes = frame.encode();
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// Bound applied to a single frame read. Defaults to the protocol
/// max (`MAX_FRAME_PAYLOAD`), but the caller can pass a tighter cap
/// for steps whose expected payload is known ahead of time (e.g.
/// IK msg2 is ~48 bytes; capping there means an attacker can't
/// declare 8KB and force an oversized allocation).
async fn read_frame_with_cap(
    stream: &mut TcpStream,
    max_payload: usize,
) -> Result<Frame, LanError> {
    let cap = max_payload.min(MAX_FRAME_PAYLOAD);
    let mut header = [0u8; FRAME_HEADER_LEN];
    stream.read_exact(&mut header).await?;
    let length = u16::from_be_bytes([header[2], header[3]]) as usize;
    if length > cap {
        return Err(LanError::OversizeFrame(length));
    }
    let mut full = vec![0u8; FRAME_HEADER_LEN + length];
    full[..FRAME_HEADER_LEN].copy_from_slice(&header);
    if length > 0 {
        stream.read_exact(&mut full[FRAME_HEADER_LEN..]).await?;
    }
    Frame::decode(&full).map_err(LanError::FrameDecode)
}

async fn read_frame(stream: &mut TcpStream) -> Result<Frame, LanError> {
    read_frame_with_cap(stream, MAX_FRAME_PAYLOAD).await
}

/// Connect to `addr` over TCP and run Noise IK + ping/pong. PRS is
/// mixed into the prologue so reconnect requires both the long-term
/// static key AND the pairwise reconnect secret.
pub async fn run_lan_reconnect(
    addr: SocketAddr,
    static_priv: &X25519SecBytes,
    peer_static_pub: &[u8; 32],
    prs: &[u8; 32],
    local_counter: u64,
    local_state: crate::core::appstate::AppState,
    wait_per_step: Duration,
    // BULK_SYNC request JSON (`{"contacts":"<sha256-hex of our cache>"}`),
    // sent after the app-state exchange. None skips the stage (CLI).
    bulk_request: Option<&str>,
) -> Result<LanReconnectOutcome, LanError> {
    info!(%addr, "TCP connecting");
    let mut stream = timeout(wait_per_step, TcpStream::connect(addr))
        .await
        .map_err(|_| LanError::Timeout("tcp connect"))??;
    // Disable Nagle — file chunks are large back-to-back frames; Nagle's
    // wait-for-ACK throttled the pull to a crawl. NODELAY ⇒ full LAN speed.
    let _ = stream.set_nodelay(true);
    info!(%addr, "TCP connected");

    let mut handshake = build_ik_initiator(static_priv, peer_static_pub, prs)?;
    let mut buf = vec![0u8; 1024];
    let mut tmp = vec![0u8; 1024];

    // ---- IK msg1 ----
    // Payload carries the local reconnect counter (M6).
    let counter_bytes = local_counter.to_be_bytes();
    let n = handshake.write_message(&counter_bytes, &mut buf)?;
    let msg1 = Frame::new(ty::RECONNECT_HANDSHAKE, 0x01, buf[..n].to_vec());
    write_frame(&mut stream, &msg1).await?;
    info!("→ IK msg1 over TCP ({} bytes, counter={local_counter})", n);

    // ---- IK msg2 ----
    // IK msg2 ≈ 32 + 16 = 48 bytes + 8-byte counter payload + 16-byte
    // tag = ~72 bytes. Cap at 128 so a hostile responder can't declare
    // a max-size allocation.
    let msg2 = timeout(wait_per_step, read_frame_with_cap(&mut stream, 128))
        .await
        .map_err(|_| LanError::Timeout("msg2"))??;
    if msg2.ty != ty::RECONNECT_HANDSHAKE || msg2.sub != 0x02 {
        return Err(LanError::UnexpectedFrame {
            ty: msg2.ty,
            sub: msg2.sub,
        });
    }
    let pt_len = handshake.read_message(&msg2.payload, &mut tmp)?;
    let peer_counter: u64 = if pt_len >= 8 {
        u64::from_be_bytes(tmp[..8].try_into().unwrap())
    } else {
        0
    };
    info!(
        "← IK msg2 over TCP ({} bytes, peer_counter={peer_counter})",
        msg2.payload.len()
    );

    let observed_pub = handshake
        .get_remote_static()
        .ok_or(LanError::NoPeerStatic)?;
    if observed_pub != peer_static_pub {
        return Err(LanError::PeerMismatch);
    }
    let transcript_hash = handshake.get_handshake_hash().to_vec();
    // Promote to Noise transport mode so we can AEAD-wrap all
    // post-handshake frames below. Must happen before `handshake` is
    // dropped — `into_transport_mode` consumes it.
    //
    // **Nonce-ordering invariant:** the `transport` cipher pair has a
    // single monotonically-increasing nonce per direction. Every
    // post-handshake `write_message` / `read_message` consumes one
    // nonce. The on-the-wire frame sequence is fixed:
    //   1. ping (write)  → pong (read)
    //   2. app-state out (write inside exchange_app_state)
    //      app-state in  (read  inside exchange_app_state)
    // Both peers MUST step through this sequence in the same order.
    // Adding a new frame type means appending it to the end (not
    // interleaving) — otherwise the nonces desynchronize and AEAD
    // decrypt fails on the next frame. The mirror Kotlin code in
    // `a3/.../LanServer.kt` follows the same ordering.
    let mut transport = handshake.into_transport_mode()?;

    // ---- ping / pong ----
    let mut nonce = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let ping = Frame::new(ty::TRANSPORT_KEEPALIVE, 0x01, nonce.to_vec());
    write_frame(&mut stream, &ping).await?;
    info!("→ ping ({})", hex::encode(nonce));

    let pong = timeout(wait_per_step, read_frame(&mut stream))
        .await
        .map_err(|_| LanError::Timeout("pong"))??;
    if pong.ty != ty::TRANSPORT_KEEPALIVE || pong.sub != 0x02 {
        return Err(LanError::UnexpectedFrame {
            ty: pong.ty,
            sub: pong.sub,
        });
    }
    if pong.payload.as_slice() != nonce {
        return Err(LanError::LivenessNonceMismatch);
    }
    info!("← pong matched");

    // ---- App-state sync (battery, device class, locale, theme, earbuds) ----
    // Pairing security is bounded by IK + ping/pong above. Each transport
    // runs its own IK per spec §8.5 — V1 does NOT use a channel join
    // proof (deferred to V2). App-level state lives in a separate
    // post-handshake exchange so we can evolve it without touching
    // pairing code.
    let peer_state = match crate::core::appstate::exchange_app_state(
        &mut stream,
        &mut transport,
        &local_state,
        wait_per_step,
    )
    .await
    {
        Ok(s) => {
            info!(
                "↔ app-state synced (peer battery={:?} class={:?})",
                s.battery, s.class
            );
            Some(s)
        }
        Err(e) => {
            tracing::warn!("app-state exchange failed: {e}");
            None
        }
    };

    // ---- Bulk-sync (BULK_SYNC 0x3D) ----
    // Hash-gated mirror transfer: we name our cached datasets' hashes, the
    // phone ships full JSON only for stale ones — over THIS reliable TCP
    // socket instead of a BLE notify burst. Skipped when the app-state
    // exchange already failed (transport unhealthy) or no request was made.
    let mut bulk: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut bulk_status: Option<BulkStatus> = None;
    if let (Some(req), Some(_)) = (bulk_request, peer_state.as_ref()) {
        match exchange_bulk(&mut stream, &mut transport, req, wait_per_step).await {
            Ok((datasets, status)) => {
                bulk = datasets;
                bulk_status = status;
            }
            Err(e) => tracing::warn!("bulk-sync exchange failed: {e}"),
        }
    }

    // Outgoing file share (laptop → phone): push a queued file now over this
    // established session, after bulk-sync, so the phone's control loop reads
    // it in nonce lock-step. FIFO-drained across heartbeats.
    if let Some(files) = crate::core::outgoing_share::take_batch() {
        if let Err(e) = push_outgoing_batch(&mut stream, &mut transport, &files).await {
            tracing::warn!("file push failed: {e}");
            crate::core::outgoing_share::report_progress(
                crate::core::outgoing_share::OutProgress::Fail,
            );
        }
    }

    let _ = stream.shutdown().await;
    Ok(LanReconnectOutcome {
        transcript_hash,
        peer_static_pub: *peer_static_pub,
        liveness_ok: true,
        remote: addr,
        peer_counter,
        peer_state,
        bulk,
        bulk_status,
    })
}

/// Seal one frame with the transport cipher and write it. The nonce advances
/// once per call — callers MUST stay in lock-step with the peer's reads.
async fn send_sealed(
    stream: &mut TcpStream,
    transport: &mut snow::TransportState,
    ty_byte: u8,
    plain: &[u8],
) -> Result<(), LanError> {
    let mut ct = vec![0u8; plain.len() + 16];
    let n = transport.write_message(plain, &mut ct)?;
    ct.truncate(n);
    let frame = Frame::new(ty_byte, 0, ct);
    stream.write_all(&frame.encode()).await?;
    stream.flush().await?;
    Ok(())
}

/// Read one transport frame within `wait` (the ciphertext is decrypted by the
/// caller). Used for the phone's FILE_PUSH_DECISION reply.
async fn recv_frame(stream: &mut TcpStream, wait: Duration) -> Result<Frame, LanError> {
    timeout(wait, read_frame(stream))
        .await
        .map_err(|_| LanError::Timeout("file-push decision"))?
}

/// Push a batch of laptop-shared files to the phone (instant-share, reverse). One
/// FILE_PUSH_OFFER lists every file (name/mime/size); the phone shows a single
/// consent prompt and replies FILE_PUSH_DECISION (1 byte: accept/decline). On
/// accept we stream each file's FILE_PUSH chunks in order — the phone delimits
/// files by each chunk stream's own `[total][idx]` header. The phone saves them
/// all to its Downloads.
async fn push_outgoing_batch(
    stream: &mut TcpStream,
    transport: &mut snow::TransportState,
    files: &[crate::core::outgoing_share::OutgoingFile],
) -> Result<(), LanError> {
    use crate::core::outgoing_share::{report_progress, OutProgress};
    let count = files.len() as u32;
    let total_bytes: u64 = files.iter().map(|f| f.bytes.len() as u64).sum();
    let label = if files.len() == 1 {
        files[0].name.clone()
    } else {
        format!("{count} files")
    };
    report_progress(OutProgress::Start {
        label,
        count,
        total: total_bytes,
    });

    // Offer: the full file list + aggregate total, so the phone can render one
    // consent prompt ("N files · X MB").
    let manifest: Vec<serde_json::Value> = files
        .iter()
        .map(|f| serde_json::json!({"name": f.name, "mime": f.mime, "bytes": f.bytes.len(), "extract": f.extract}))
        .collect();
    let offer = serde_json::json!({
        "files": manifest, "count": count, "total": total_bytes,
    })
    .to_string();
    if let Err(e) = send_sealed(stream, transport, ty::FILE_PUSH_OFFER, offer.as_bytes()).await {
        report_progress(OutProgress::Fail);
        return Err(e);
    }

    // Consent: wait for the phone user's Accept/Decline (the phone caps its own
    // prompt at ~45 s; give it 60 s before we treat silence as a decline).
    let decision = recv_frame(stream, Duration::from_secs(60)).await?;
    if decision.ty != ty::FILE_PUSH_DECISION {
        report_progress(OutProgress::Declined);
        return Err(LanError::Timeout("file-push decision (unexpected frame)"));
    }
    let mut pt = vec![0u8; decision.payload.len()];
    let n = transport.read_message(&decision.payload, &mut pt)?;
    pt.truncate(n);
    let accepted = pt.first().copied() == Some(1);
    if !accepted {
        report_progress(OutProgress::Declined);
        info!("← file push declined by phone ({count} files)");
        return Ok(());
    }
    report_progress(OutProgress::Accepted);

    // Stream every file's chunks in order. Cumulative byte counter drives the
    // aggregate pill.
    let mut sent: u64 = 0;
    for (fi, f) in files.iter().enumerate() {
        for payload in crate::core::outgoing_share::build_chunks(&f.bytes) {
            // chunk payload = [total u16][idx u16][data]; bytes shipped this
            // frame = payload.len() - 4.
            let data_len = payload.len().saturating_sub(4) as u64;
            if let Err(e) = send_sealed(stream, transport, ty::FILE_PUSH, &payload).await {
                report_progress(OutProgress::Fail);
                return Err(e);
            }
            sent += data_len;
            report_progress(OutProgress::Progress {
                sent,
                total: total_bytes,
            });
        }
        info!("→ file push [{}/{count}] '{}' ({} bytes)", fi + 1, f.name, f.bytes.len());
    }
    report_progress(OutProgress::Done);
    info!("→ file push batch done ({count} files, {total_bytes} bytes)");
    Ok(())
}

/// Send the bulk-sync request and collect the chunked dataset frames the
/// phone ships back for stale hashes, until its done frame (sub 0x02) or
/// the time budget runs out. An old phone build that doesn't know
/// BULK_SYNC simply never answers — the timeout path degrades gracefully.
async fn exchange_bulk(
    stream: &mut TcpStream,
    transport: &mut snow::TransportState,
    request_json: &str,
    wait: Duration,
) -> Result<(Vec<(u8, Vec<u8>)>, Option<BulkStatus>), LanError> {
    let plain = request_json.as_bytes();
    let mut ct = vec![0u8; plain.len() + 16];
    let n = transport.write_message(plain, &mut ct)?;
    ct.truncate(n);
    let frame = Frame::new(ty::BULK_SYNC, 0x01, ct);
    stream.write_all(&frame.encode()).await?;
    stream.flush().await?;
    info!("→ bulk-sync request ({} bytes)", plain.len());

    let mut out: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut status: Option<BulkStatus> = None;
    let mut contacts = crate::core::contacts::ContactsAssembler::default();
    let mut call_log = crate::core::call_log::CallLogAssembler::default();
    let mut sms = crate::core::sms::SmsAssembler::default();
    let mut sms_history = crate::core::sms::SmsAssembler::default();
    let mut call_log_history = crate::core::call_log::CallLogAssembler::default();
    let mut sms_ids = crate::core::sms::SmsAssembler::default();
    let mut clipboard_image = crate::core::clipboard_mirror::ImageAssembler::default();
    let mut clipboard_file = crate::core::clipboard_mirror::ImageAssembler::default();
    let mut file_chunks_seen: u32 = 0; // for incoming-file progress reporting
    let mut file_start: Option<tokio::time::Instant> = None; // for throughput log
    // SLIDING idle budget: reset on every frame, so a large file pull that keeps
    // making progress never gets cut off (the old fixed 60 s cap made a >~40 MB
    // file on a slow link time out → re-request from scratch → loop forever).
    // A genuinely stalled transfer still aborts after IDLE with no frame.
    const IDLE: Duration = Duration::from_secs(30);
    let mut deadline = tokio::time::Instant::now() + IDLE;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            tracing::warn!("bulk-sync: idle (no frame for 30s) before done; stopping");
            break;
        }
        let frame = match timeout(remaining.min(wait), read_frame(stream)).await {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => {
                tracing::warn!("bulk-sync read failed: {e}");
                break;
            }
            Err(_) => {
                info!("bulk-sync: no response (phone predates BULK_SYNC?); skipping");
                break;
            }
        };
        // Progress made → slide the idle deadline forward.
        deadline = tokio::time::Instant::now() + IDLE;
        let mut pt = vec![0u8; frame.payload.len()];
        let n = transport.read_message(&frame.payload, &mut pt)?;
        pt.truncate(n);
        match frame.ty {
            ty::BULK_SYNC if frame.sub == 0x02 => {
                info!("← bulk-sync done: {}", String::from_utf8_lossy(&pt));
                status = BulkStatus::parse(&pt);
                if status.is_none() {
                    tracing::warn!("bulk-sync: done frame is not a JSON object; no status");
                }
                break;
            }
            ty::CONTACTS => {
                if let Some((total, idx, data)) = crate::core::contacts::parse_chunk(&pt) {
                    if let Some(json) = contacts.add(total, idx, data) {
                        info!("← bulk-sync contacts ({} bytes)", json.len());
                        out.push((ty::CONTACTS, json));
                    }
                } else {
                    tracing::warn!("bulk-sync: malformed contacts chunk; dropping");
                }
            }
            ty::CALL_LOG => {
                if let Some((total, idx, data)) = crate::core::call_log::parse_chunk(&pt) {
                    if let Some(json) = call_log.add(total, idx, data) {
                        info!("← bulk-sync call log ({} bytes)", json.len());
                        out.push((ty::CALL_LOG, json));
                    }
                } else {
                    tracing::warn!("bulk-sync: malformed call-log chunk; dropping");
                }
            }
            ty::SMS => {
                if let Some((total, idx, data)) = crate::core::sms::parse_chunk(&pt) {
                    if let Some(json) = sms.add(total, idx, data) {
                        info!("← bulk-sync sms ({} bytes)", json.len());
                        out.push((ty::SMS, json));
                    }
                } else {
                    tracing::warn!("bulk-sync: malformed sms chunk; dropping");
                }
            }
            // History batches (watermark datasets) — merge semantics on the
            // UI side, hence the distinct frame types.
            ty::SMS_THREAD => {
                if let Some((total, idx, data)) = crate::core::sms::parse_chunk(&pt) {
                    if let Some(json) = sms_history.add(total, idx, data) {
                        info!("← bulk-sync sms history ({} bytes)", json.len());
                        out.push((ty::SMS_THREAD, json));
                    }
                } else {
                    tracing::warn!("bulk-sync: malformed sms-history chunk; dropping");
                }
            }
            ty::CALL_LOG_HISTORY => {
                if let Some((total, idx, data)) = crate::core::call_log::parse_chunk(&pt) {
                    if let Some(json) = call_log_history.add(total, idx, data) {
                        info!("← bulk-sync call-log history ({} bytes)", json.len());
                        out.push((ty::CALL_LOG_HISTORY, json));
                    }
                } else {
                    tracing::warn!("bulk-sync: malformed call-log-history chunk; dropping");
                }
            }
            ty::SMS_IDS => {
                if let Some((total, idx, data)) = crate::core::sms::parse_chunk(&pt) {
                    if let Some(json) = sms_ids.add(total, idx, data) {
                        info!("← bulk-sync sms ids ({} bytes)", json.len());
                        out.push((ty::SMS_IDS, json));
                    }
                } else {
                    tracing::warn!("bulk-sync: malformed sms-ids chunk; dropping");
                }
            }
            ty::CLIPBOARD_IMAGE => {
                // Reliable LAN pull of a phone-shared image (vs the lossy BLE
                // chunk path). `[total][idx][data]` like the other datasets.
                if let Some((total, idx, data)) = crate::core::sms::parse_chunk(&pt) {
                    if let Some(png) = clipboard_image.add(total, idx, data) {
                        info!("← bulk-sync clipboard image ({} bytes)", png.len());
                        out.push((ty::CLIPBOARD_IMAGE, png));
                    }
                } else {
                    tracing::warn!("bulk-sync: malformed clipboard-image chunk; dropping");
                }
            }
            ty::CLIPBOARD_FILE => {
                // Instant-share-style file pull. The assembler resets on completion, so
                // several files served back-to-back reassemble independently.
                if let Some((total, idx, data)) = crate::core::sms::parse_chunk(&pt) {
                    file_chunks_seen = file_chunks_seen.saturating_add(1);
                    if file_chunks_seen == 1 {
                        file_start = Some(tokio::time::Instant::now());
                    }
                    // Live progress for the transfer panel (chunks → %).
                    crate::core::file_progress::report(file_chunks_seen, total as u32);
                    if let Some(blob) = clipboard_file.add(total, idx, data) {
                        let secs = file_start
                            .map(|s| s.elapsed().as_secs_f64())
                            .unwrap_or(0.0)
                            .max(0.001);
                        let mbps = blob.len() as f64 / 1024.0 / 1024.0 / secs;
                        info!(
                            "← bulk-sync file ({} bytes) in {:.2}s = {:.1} MB/s",
                            blob.len(),
                            secs,
                            mbps
                        );
                        out.push((ty::CLIPBOARD_FILE, blob));
                        file_chunks_seen = 0; // reset for the next file in the stream
                        file_start = None;
                    }
                } else {
                    tracing::warn!("bulk-sync: malformed clipboard-file chunk; dropping");
                }
            }
            other => {
                tracing::warn!("bulk-sync: unexpected frame 0x{other:02x}; ignoring");
            }
        }
    }
    Ok((out, status))
}


#[cfg(test)]
mod tests {
    use super::BulkStatus;

    #[test]
    fn nomatch_is_unservable_and_sent_is_not() {
        let s = BulkStatus::parse(br#"{"contacts":"match","clipboard_file":"nomatch"}"#).unwrap();
        assert!(s.unservable("clipboard_file"));
        assert_eq!(s.get("contacts"), Some("match"));

        let s = BulkStatus::parse(br#"{"clipboard_file":"sent"}"#).unwrap();
        assert!(!s.unservable("clipboard_file"));
    }

    #[test]
    fn a_dataset_the_phone_said_nothing_about_is_not_unservable() {
        // Never drop a queued pull on silence — only on an explicit answer.
        let s = BulkStatus::parse(br#"{"contacts":"match"}"#).unwrap();
        assert!(!s.unservable("clipboard_file"));
        assert_eq!(s.get("clipboard_file"), None);
    }

    #[test]
    fn errors_count_as_unservable_and_junk_parses_to_none() {
        let s = BulkStatus::parse(br#"{"clipboard_file":"error"}"#).unwrap();
        assert!(s.unservable("clipboard_file"));
        // Non-string values are skipped, not fatal.
        let s = BulkStatus::parse(br#"{"clipboard_file":"nomatch","n":7}"#).unwrap();
        assert!(s.unservable("clipboard_file"));
        assert!(BulkStatus::parse(b"[]").is_none());
        assert!(BulkStatus::parse(b"not json").is_none());
    }
}
