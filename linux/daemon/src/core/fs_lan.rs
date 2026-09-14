//! A LAN transport for the filesystem protocol.
//!
//! BLE carries these frames today and works, but at 30-40 KiB/s (measured) —
//! fine for a directory listing, hopeless for content. Design doc §6 is
//! explicit that content streams over Wi-Fi and BLE stays for metadata and
//! wake-up, so this opens a TCP+IK session and pushes the same frames down it.
//!
//! Two properties make it worth a dedicated session rather than reusing the
//! heartbeat:
//!
//! * **It stays open.** The heartbeat connects, syncs and disconnects every
//!   ~13 s. Filesystem work is bursty and correlated — a fetch is one OPEN, N
//!   READs and a CLOSE — and paying a TCP connect plus an IK handshake
//!   (~300 ms on this link) per request would cost more than the reads.
//! * **No fragmentation.** A BLE notify caps at 512 bytes, so a 48 KiB read is
//!   96 fragments paced 10 ms apart. Over TCP the same read is one frame.
//!
//! The session is otherwise deliberately dumb: it moves frames and knows
//! nothing about ops, ids or handles. The caller keeps all of that, which is
//! what lets the same `fs_link` client sit on either transport.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::core::ble::frame::{ty, Frame, FRAME_HEADER_LEN, MAX_FRAME_PAYLOAD};
use crate::core::crypto::x25519::X25519SecBytes;

/// Budget for the handshake. Same shape as the audio session's: generous
/// enough for a sleepy phone, short enough that a dead address fails fast and
/// the caller can fall back to BLE while the user is still watching.
const IK_STEP_TIMEOUT: Duration = Duration::from_secs(8);

/// Writes one sealed frame onto the session.
pub type FsLanWriter = Arc<
    dyn Fn(u8, u8, Vec<u8>) -> futures::future::BoxFuture<'static, Result<(), String>>
        + Send
        + Sync,
>;

/// Open a TCP+IK session for filesystem frames.
///
/// On success the read loop runs until the socket closes or a frame fails to
/// open, handing every decrypted frame to `on_frame` and calling `on_closed`
/// exactly once at the end — the caller uses that to drop its cached writer so
/// the next request reopens or falls back rather than writing into a dead
/// socket.
#[allow(clippy::too_many_arguments)]
pub async fn open_session(
    addr: SocketAddr,
    static_priv: &X25519SecBytes,
    peer_static_pub: &[u8; 32],
    prs: &[u8; 32],
    local_counter: u64,
    on_frame: Arc<dyn Fn(Frame) + Send + Sync>,
    on_closed: Arc<dyn Fn() + Send + Sync>,
) -> Result<FsLanWriter, String> {
    let mut stream = timeout(IK_STEP_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| "tcp connect timeout".to_string())?
        .map_err(|e| format!("tcp connect: {e}"))?;
    // Filesystem traffic is many small round trips (a stat per icon) plus a
    // few large ones. Nagle would sit on the small ones waiting for company.
    let _ = stream.set_nodelay(true);

    let mut handshake =
        crate::core::lan::tcp_client::build_ik_initiator(static_priv, peer_static_pub, prs)
            .map_err(|e| format!("noise build: {e}"))?;
    let mut buf = vec![0u8; 1024];
    let mut tmp = vec![0u8; 1024];

    let n = handshake
        .write_message(&local_counter.to_be_bytes(), &mut buf)
        .map_err(|e| format!("noise write msg1: {e}"))?;
    write_frame(&mut stream, &Frame::new(ty::RECONNECT_HANDSHAKE, 0x01, buf[..n].to_vec())).await?;

    let msg2 = timeout(IK_STEP_TIMEOUT, read_frame_capped(&mut stream, 128))
        .await
        .map_err(|_| "msg2 timeout".to_string())??;
    if msg2.ty != ty::RECONNECT_HANDSHAKE || msg2.sub != 0x02 {
        return Err(format!("unexpected msg2 ty=0x{:02x}", msg2.ty));
    }
    handshake
        .read_message(&msg2.payload, &mut tmp)
        .map_err(|e| format!("noise read msg2: {e}"))?;

    // The peer's static must be the one we trusted at pair time. Checked
    // before a single filesystem frame goes out: this session can be asked to
    // read the user's files, so "who is on the other end" is not a question to
    // answer optimistically.
    if handshake
        .get_remote_static()
        .ok_or_else(|| "no remote static after IK".to_string())?
        != peer_static_pub
    {
        return Err("peer static mismatch".to_string());
    }

    let transport = Arc::new(Mutex::new(
        handshake
            .into_transport_mode()
            .map_err(|e| format!("transport mode: {e}"))?,
    ));

    let (mut reader, writer_half) = stream.into_split();
    let writer_half = Arc::new(Mutex::new(writer_half));

    // Read loop. Owns the receive side of the cipher, so it never contends
    // with the writer for it beyond the shared mutex.
    {
        let transport = transport.clone();
        tokio::spawn(async move {
            loop {
                match read_sealed(&mut reader, &transport).await {
                    Ok(Some(frame)) => on_frame(frame),
                    Ok(None) => {
                        tracing::info!("fs-lan: peer closed the session");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("fs-lan: read loop ended: {e}");
                        break;
                    }
                }
            }
            on_closed();
        });
    }

    let writer: FsLanWriter = Arc::new(move |ty_byte: u8, sub: u8, plain: Vec<u8>| {
        let transport = transport.clone();
        let writer_half = writer_half.clone();
        Box::pin(async move {
            if plain.len() + 16 > MAX_FRAME_PAYLOAD {
                return Err(format!("frame too large: {}", plain.len()));
            }
            let mut out = vec![0u8; plain.len() + 16];
            let n = {
                let mut t = transport.lock().await;
                t.write_message(&plain, &mut out)
                    .map_err(|e| format!("aead seal: {e}"))?
            };
            let bytes = Frame::new(ty_byte, sub, out[..n].to_vec()).encode();
            let mut w = writer_half.lock().await;
            w.write_all(&bytes).await.map_err(|e| format!("tcp write: {e}"))?;
            w.flush().await.map_err(|e| format!("tcp flush: {e}"))?;
            Ok(())
        })
    });

    tracing::info!(%addr, "fs-lan: session up");
    Ok(writer)
}

async fn write_frame(stream: &mut TcpStream, frame: &Frame) -> Result<(), String> {
    let bytes = frame.encode();
    stream.write_all(&bytes).await.map_err(|e| format!("tcp write: {e}"))?;
    stream.flush().await.map_err(|e| format!("tcp flush: {e}"))?;
    Ok(())
}

async fn read_frame_capped(stream: &mut TcpStream, cap: usize) -> Result<Frame, String> {
    let cap = cap.min(MAX_FRAME_PAYLOAD);
    let mut header = [0u8; FRAME_HEADER_LEN];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|e| format!("tcp read header: {e}"))?;
    let length = u16::from_be_bytes([header[2], header[3]]) as usize;
    if length > cap {
        return Err(format!("oversize frame {length}"));
    }
    let mut full = vec![0u8; FRAME_HEADER_LEN + length];
    full[..FRAME_HEADER_LEN].copy_from_slice(&header);
    if length > 0 {
        stream
            .read_exact(&mut full[FRAME_HEADER_LEN..])
            .await
            .map_err(|e| format!("tcp read body: {e}"))?;
    }
    Frame::decode(&full).map_err(|e| format!("frame decode: {e}"))
}

/// Read one frame and AEAD-open it. `Ok(None)` is a clean EOF.
async fn read_sealed(
    reader: &mut tokio::net::tcp::OwnedReadHalf,
    transport: &Arc<Mutex<snow::TransportState>>,
) -> Result<Option<Frame>, String> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(format!("tcp read header: {e}")),
    }
    let length = u16::from_be_bytes([header[2], header[3]]) as usize;
    if length > MAX_FRAME_PAYLOAD {
        return Err(format!("oversize frame {length}"));
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        reader
            .read_exact(&mut body)
            .await
            .map_err(|e| format!("tcp read body: {e}"))?;
    }
    let mut plain = vec![0u8; length.max(16)];
    let n = {
        let mut t = transport.lock().await;
        t.read_message(&body, &mut plain)
            .map_err(|e| format!("aead open: {e}"))?
    };
    Ok(Some(Frame::new(header[0], header[1], plain[..n].to_vec())))
}
