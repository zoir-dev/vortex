//! TCP video data-plane for screen mirroring — reliable + ordered, like scrcpy.
//!
//! Replaces the lossy UDP path. On Wi-Fi a bursty UDP fragment train gets its
//! tail dropped, and since one lost fragment kills a whole H.264 access unit,
//! that surfaces as the visible "freeze then jump". TCP retransmits invisibly,
//! so the stream never loses a frame — the same reason scrcpy and the upstream
//! `ecosystem` mirror used TCP for video.
//!
//! The **laptop is the client**: its firewall blocks unsolicited inbound but
//! allows outbound + established, so it connects OUT to the phone's video server
//! on [`VIDEO_PORT`]. Each access unit is sealed whole (no fragmentation needed
//! over a byte stream) with ChaCha20-Poly1305 under a monotonic counter nonce,
//! using the same media key as the old UDP path (`mirror_udp::derive_media_key`).
//!
//! Wire (phone → laptop), repeated back-to-back:
//! ```text
//!   [ msg_len : u32 BE ]                  # = 8 + ciphertext length
//!   [ counter : u64 BE ]                  # nonce material + AEAD AAD
//!   [ ChaCha20Poly1305(key, nonce = 0u32 || counter, aad = counter, AU) ]
//! ```

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// Fixed TCP port the phone serves video on; the laptop connects out to it.
/// MUST match the Android `ScreenMirrorService` video server port.
pub const VIDEO_PORT: u16 = 51822;

/// Fixed TCP port the PHONE serves its laptop-screen viewer on (laptop→phone
/// mirror); the laptop connects out to it, exactly as for [`VIDEO_PORT`]. MUST
/// match the Android `LaptopMirrorClient` port.
///
/// The name is historical and reads backwards: it is the port used *for* the
/// laptop's screen, not a port the laptop listens on. Every video path here is
/// dialled outward from the laptop — see the module note above — so Vortex needs
/// no inbound firewall rule for any of them. (Said plainly because the previous
/// wording claimed the laptop served this port, which sent one debugging session
/// off after a firewall that was never involved.)
pub const LAPTOP_VIDEO_PORT: u16 = 51823;

/// Fixed TCP port the phone serves its CAMERA on (phone-as-webcam); the laptop
/// connects out to it and pipes the decoded frames into a v4l2loopback device.
/// MUST match the Android `CameraStreamService` port.
pub const CAMERA_VIDEO_PORT: u16 = 51824;

/// Reject absurd frame lengths (guards against a corrupt/hostile length prefix
/// turning into a multi-GB allocation).
const MAX_AU_LEN: usize = 8 * 1024 * 1024;

/// 12-byte nonce from the 8-byte counter (high 4 bytes zero) — matches the
/// phone's `MirrorTcpSealer` and the old UDP scheme.
#[inline]
fn nonce_from_counter(counter: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    n
}

/// Connect to the phone's video server on [`VIDEO_PORT`] (screen mirror) and
/// stream decrypted access units to `au_tx`. Thin wrapper over
/// [`run_tcp_video_receiver_on`] for the default screen-mirror port.
pub async fn run_tcp_video_receiver(
    phone_ip: IpAddr,
    key: [u8; 32],
    au_tx: mpsc::Sender<Vec<u8>>,
    keyframe_tx: Option<mpsc::Sender<()>>,
) {
    run_tcp_video_receiver_on(phone_ip, VIDEO_PORT, key, au_tx, keyframe_tx).await;
}

/// Connect to the phone's video server on `port` (retrying — it only opens
/// AFTER the user grants the on-phone capture consent) and stream decrypted
/// access units to `au_tx` until the connection closes or the consumer is gone.
/// Used for screen mirror ([`VIDEO_PORT`]) and the phone-as-webcam ([`CAMERA_VIDEO_PORT`]).
pub async fn run_tcp_video_receiver_on(
    phone_ip: IpAddr,
    port: u16,
    key: [u8; 32],
    au_tx: mpsc::Sender<Vec<u8>>,
    keyframe_tx: Option<mpsc::Sender<()>>,
) {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let addr = SocketAddr::new(phone_ip, port);

    // Retry connect for ~60 s: the phone's ServerSocket appears only once the
    // user taps "Start now", which can take several seconds.
    let mut stream = None;
    for attempt in 0..120u32 {
        match TcpStream::connect(addr).await {
            Ok(s) => {
                let _ = s.set_nodelay(true); // Nagle off — low latency
                stream = Some(s);
                break;
            }
            Err(e) => {
                if attempt == 0 {
                    tracing::info!(%addr, "mirror tcp: waiting for phone video server ({e})");
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    let Some(mut stream) = stream else {
        tracing::warn!(%addr, "mirror tcp: phone video server never came up");
        return;
    };
    tracing::info!(%addr, "mirror tcp: video connected");

    let mut len_buf = [0u8; 4];
    let mut aus: u64 = 0;
    let mut dropped: u64 = 0;
    loop {
        if let Err(e) = stream.read_exact(&mut len_buf).await {
            tracing::info!("mirror tcp: video stream ended ({e})");
            break;
        }
        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len < 8 + 16 || msg_len > MAX_AU_LEN {
            tracing::warn!(msg_len, "mirror tcp: bad frame length — closing");
            break;
        }
        let mut msg = vec![0u8; msg_len];
        if let Err(e) = stream.read_exact(&mut msg).await {
            tracing::info!("mirror tcp: short read ({e})");
            break;
        }
        let counter = u64::from_be_bytes(msg[..8].try_into().unwrap());
        let nonce = nonce_from_counter(counter);
        let au = match cipher.decrypt(
            Nonce::from_slice(&nonce),
            Payload { msg: &msg[8..], aad: &msg[..8] },
        ) {
            Ok(p) => p,
            Err(_) => {
                tracing::warn!(counter, "mirror tcp: AEAD open failed — closing");
                break;
            }
        };
        aus += 1;
        if aus == 1 {
            tracing::info!(bytes = au.len(), "mirror tcp: FIRST access unit → GStreamer");
        }
        if aus % 120 == 0 {
            tracing::info!(aus, "mirror tcp: streaming");
        }
        // NEVER await room here. This loop is the only thing draining the
        // socket, so blocking on a full channel stops reading, the phone's
        // send buffer fills, and its encoder thread parks in sendto() — which
        // is how a laptop-side display hiccup turned into a frozen phone and
        // an ANR when the service was torn down. A live mirror would rather
        // lose a frame than stall the sender: drop, and ask for an IDR so the
        // decoder resyncs at once instead of showing garbage until the next
        // scheduled keyframe.
        match au_tx.try_send(au) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!("mirror tcp: AU consumer gone — stopping");
                break;
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                dropped += 1;
                if dropped % 30 == 1 {
                    tracing::warn!(dropped, "mirror tcp: decoder behind — dropping frames");
                }
                if let Some(kf) = keyframe_tx.as_ref() {
                    let _ = kf.try_send(()); // rate-limited on the writer side
                }
            }
        }
    }
    tracing::info!(aus, "mirror tcp: video receiver ended");
}

// ===========================================================================
// LAPTOP → phone direction (SENDER): seal the laptop's own H.265 access units
// and serve them on [`LAPTOP_VIDEO_PORT`]. Same wire as above; the phone is the
// client and opens with `mirror_udp::derive_laptop_media_key`.
// ===========================================================================

/// Seals whole H.265 access units for the laptop→phone TCP stream. One sealer
/// per cast session; `counter` is monotonic (never reset, not even across a
/// phone reconnect) so the ChaCha20-Poly1305 nonce never repeats under one key.
pub struct MirrorTcpSealer {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl MirrorTcpSealer {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
            counter: 0,
        }
    }

    /// Seal one access unit into a ready-to-write framed message:
    /// `[msg_len u32 BE][counter u64 BE][ChaCha20Poly1305(nonce=0u32||counter, aad=counter, AU)]`.
    pub fn seal(&mut self, au: &[u8]) -> Vec<u8> {
        let counter = self.counter;
        self.counter = self.counter.wrapping_add(1);
        let counter_be = counter.to_be_bytes();
        let nonce = nonce_from_counter(counter);
        let ct = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload { msg: au, aad: &counter_be },
            )
            .expect("chacha seal");
        let msg_len = (8 + ct.len()) as u32;
        let mut out = Vec::with_capacity(4 + 8 + ct.len());
        out.extend_from_slice(&msg_len.to_be_bytes());
        out.extend_from_slice(&counter_be);
        out.extend_from_slice(&ct);
        out
    }
}

/// Push the laptop's screen TO the phone: the laptop is the CLIENT here (it
/// connects OUT to the phone's viewer server on [`LAPTOP_VIDEO_PORT`]) because
/// on real networks only laptop→phone connections succeed — the phone is often
/// unreachable inbound (different subnet / AP isolation), exactly why the
/// phone→laptop mirror also has the laptop dial the phone. Seals each access
/// unit pulled from `au_rx` and writes it until the phone drops or `au_rx`
/// closes (cast stopped). Retries the connect for ~60s while the phone brings
/// its viewer server up after accepting the request.
pub async fn run_tcp_video_client(
    phone_ip: IpAddr,
    key: [u8; 32],
    mut au_rx: mpsc::Receiver<Vec<u8>>,
) {
    let addr = SocketAddr::new(phone_ip, LAPTOP_VIDEO_PORT);
    // ONE sealer for the whole cast: the counter stays monotonic ACROSS
    // reconnects so the ChaCha nonce never repeats under this key.
    let mut sealer = MirrorTcpSealer::new(&key);

    // Reconnect loop: the link can drop transiently (network blip, the phone
    // re-creating its viewer). Self-heal by dialing again — the phone's viewer
    // server re-accepts and resyncs on the next keyframe (~1s). Exit only when
    // `au_rx` closes (cast stopped) or the connect retries are exhausted.
    'session: loop {
        let mut stream = None;
        for attempt in 0..120u32 {
            match TcpStream::connect(addr).await {
                Ok(s) => {
                    let _ = s.set_nodelay(true);
                    stream = Some(s);
                    break;
                }
                Err(e) => {
                    if attempt == 0 {
                        tracing::info!(%addr, "laptop-cast: waiting for phone viewer server ({e})");
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    // Give up retrying if the cast was stopped while we waited.
                    if au_rx.try_recv().is_err() && au_rx.is_closed() {
                        return;
                    }
                }
            }
        }
        let Some(mut stream) = stream else {
            tracing::warn!(%addr, "laptop-cast: phone viewer never came up");
            return;
        };
        tracing::info!(%addr, "laptop-cast: connected to phone viewer — streaming");

        // Discard anything buffered while (re)connecting so we begin near-live;
        // the phone waits for the next keyframe anyway.
        while au_rx.try_recv().is_ok() {}

        let mut sent: u64 = 0;
        loop {
            match au_rx.recv().await {
                Some(au) => {
                    let msg = sealer.seal(&au);
                    if let Err(e) = stream.write_all(&msg).await {
                        tracing::info!(sent, "laptop-cast: phone viewer dropped ({e}) — reconnecting");
                        continue 'session; // self-heal: dial again
                    }
                    sent += 1;
                }
                None => {
                    tracing::info!(sent, "laptop-cast: cast stopped — video push ended");
                    return; // au_rx closed → cast torn down
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::mirror_udp::derive_laptop_media_key;

    /// The sealer's wire must round-trip through the same open logic the phone
    /// uses: framing, counter-as-nonce, counter-as-AAD all line up.
    #[test]
    fn sealer_roundtrips_through_open() {
        let key = derive_laptop_media_key(b"test-handshake-hash");
        let mut sealer = MirrorTcpSealer::new(&key);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));

        for (i, au) in [b"first-access-unit".as_ref(), b"second", b""].iter().enumerate() {
            let msg = sealer.seal(au);
            // Parse the frame exactly like `run_tcp_video_receiver`.
            let msg_len = u32::from_be_bytes(msg[..4].try_into().unwrap()) as usize;
            assert_eq!(msg_len, msg.len() - 4);
            let body = &msg[4..];
            let counter = u64::from_be_bytes(body[..8].try_into().unwrap());
            assert_eq!(counter, i as u64, "counter must be monotonic from 0");
            let nonce = nonce_from_counter(counter);
            let opened = cipher
                .decrypt(
                    Nonce::from_slice(&nonce),
                    Payload { msg: &body[8..], aad: &body[..8] },
                )
                .expect("open must succeed");
            assert_eq!(&opened, au);
        }
    }

    /// The two directions MUST derive different keys (else nonce reuse).
    #[test]
    fn laptop_key_differs_from_phone_key() {
        let h = b"same-handshake";
        assert_ne!(
            super::super::mirror_udp::derive_media_key(h),
            derive_laptop_media_key(h),
        );
    }
}
