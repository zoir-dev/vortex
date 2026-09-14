//! Prefer Wi-Fi for filesystem traffic; fall back to BLE.
//!
//! Both transports carry the same frames, so this is purely a routing
//! decision. It matters because they are three orders of magnitude apart: BLE
//! measured 30-41 KiB/s on this link, and a 48 KiB read has to go out as ~96
//! notify fragments paced 10 ms apart. Over TCP it is one frame.
//!
//! The session is opened lazily on the first filesystem op and kept while it is
//! being used, because filesystem work arrives in correlated bursts — a fetch
//! is OPEN, N READs, CLOSE — and a TCP connect plus IK per request would cost
//! more than the reads it carried. It is dropped after [`IDLE_TIMEOUT`] so a
//! browse that ended does not hold the phone's Wi-Fi awake.

use std::sync::Arc;
use std::time::{Duration, Instant};

use vortex_l3_daemon::core::fs_lan::FsLanWriter;
use vortex_l3_daemon::core::identity::IdentityRecord;
use vortex_l3_daemon::core::storage::peers::PeerStore;

/// How long an unused session is kept before it is closed.
///
/// Long enough to cover a user clicking through folders, short enough that an
/// abandoned browse stops holding a socket (and the phone's radio) open.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Don't retry a failed connect more often than this.
///
/// Without it, every request in a burst would pay the full connect timeout
/// before falling back — turning one unreachable phone into a stall per read
/// instead of a single one.
const RETRY_COOLDOWN: Duration = Duration::from_secs(30);

struct Ctx {
    identity: IdentityRecord,
    peer_store: Arc<dyn PeerStore>,
}

static CTX: std::sync::OnceLock<Ctx> = std::sync::OnceLock::new();

#[derive(Default)]
struct Session {
    writer: Option<FsLanWriter>,
    last_used: Option<Instant>,
    last_failed: Option<Instant>,
    /// Whether the user has already been told about this outage, so a browse
    /// that issues fifty reads over a dead network produces one notification
    /// rather than fifty.
    warned: bool,
}

fn session() -> &'static tokio::sync::Mutex<Session> {
    static S: std::sync::OnceLock<tokio::sync::Mutex<Session>> = std::sync::OnceLock::new();
    S.get_or_init(|| tokio::sync::Mutex::new(Session::default()))
}

/// Supply the credentials a session needs. Called once, alongside
/// [`crate::fs_link::init`].
pub(crate) fn init(identity: IdentityRecord, peer_store: Arc<dyn PeerStore>) {
    let _ = CTX.set(Ctx {
        identity,
        peer_store,
    });
}

/// The LAN writer, opening a session if needed. `None` means "use BLE".
pub(crate) async fn writer() -> Option<FsLanWriter> {
    let mut s = session().lock().await;

    // Reuse a live session, unless it has been idle long enough that the peer
    // may have dropped it under us.
    if let Some(w) = s.writer.clone() {
        if s.last_used.is_some_and(|t| t.elapsed() < IDLE_TIMEOUT) {
            s.last_used = Some(Instant::now());
            return Some(w);
        }
        tracing::info!("fs-lan: closing idle session");
        s.writer = None;
    }
    if s.last_failed.is_some_and(|t| t.elapsed() < RETRY_COOLDOWN) {
        return None;
    }

    let Some(ctx) = CTX.get() else { return None };
    let peers = {
        let store = ctx.peer_store.clone();
        match tokio::task::spawn_blocking(move || store.list().unwrap_or_default()).await {
            Ok(v) => v,
            Err(_) => return None,
        }
    };
    // The peer whose files we are browsing is the active one, for the same
    // reason the BLE loop dials it: it owns the session the UI is showing.
    let Some(peer) = crate::arbiter::preferred_peer(peers) else { return None };

    // Every "cannot use LAN" path from here on records the failure, rather
    // than only the ones that get as far as a failed handshake. Resolving the
    // address is itself expensive — a probe plus an mDNS browse, measured at
    // 12 s with the phone's Wi-Fi off — and returning early without arming the
    // cooldown made every send in a burst pay it again. An eleven-read fetch
    // would have spent over two minutes rediscovering a phone that was not
    // there, before falling back each time.
    let Some(addr) = crate::lan::resolve_peer_addr(false).await else {
        tracing::info!("fs-lan: phone not reachable on the network; using BLE");
        note_failure(&mut s);
        return None;
    };
    let local_counter = {
        let store = ctx.peer_store.clone();
        let peer_pub = peer.peer_static_pub;
        tokio::task::spawn_blocking(move || store.load_counter(&peer_pub).unwrap_or(0))
            .await
            .unwrap_or(0)
    };

    let peer_pub = peer.peer_static_pub;
    let on_frame = Arc::new(move |f: vortex_l3_daemon::core::ble::frame::Frame| {
        // Same entry point the BLE listener uses, so a reply is handled
        // identically whichever transport carried it.
        crate::fs_link::dispatch(vortex_l3_daemon::core::ble::frame::RawFrame {
            peer_pub,
            ty: f.ty,
            sub: f.sub,
            payload: f.payload,
        });
    });
    let on_closed = Arc::new(|| {
        tokio::spawn(async {
            let mut s = session().lock().await;
            s.writer = None;
            s.last_used = None;
        });
    });

    match vortex_l3_daemon::core::fs_lan::open_session(
        addr,
        &ctx.identity.static_priv.0,
        &peer.peer_static_pub,
        &peer.prs,
        local_counter,
        on_frame,
        on_closed,
    )
    .await
    {
        Ok(w) => {
            s.writer = Some(w.clone());
            s.last_used = Some(Instant::now());
            s.last_failed = None;
            s.warned = false;
            Some(w)
        }
        Err(e) => {
            tracing::warn!(%addr, "fs-lan: session failed ({e}); using BLE");
            note_failure(&mut s);
            None
        }
    }
}

/// Arm the retry cooldown and tell the user once.
///
/// One place, so a new "give up on LAN" branch cannot forget either half. The
/// first version warned only on a failed handshake, which missed the common
/// case entirely: with the phone off Wi-Fi there is no address to hand to a
/// handshake, so the user got a silent 20x slowdown and no cooldown.
fn note_failure(s: &mut Session) {
    s.last_failed = Some(Instant::now());
    if !s.warned {
        s.warned = true;
        warn_user_slow_link();
    }
}

/// Drop the session — the peer changed, or the link went.
pub(crate) async fn close() {
    let mut s = session().lock().await;
    if s.writer.take().is_some() {
        tracing::info!("fs-lan: session closed");
    }
    s.last_used = None;
}

/// Tell the user why browsing just got slow.
///
/// Worth interrupting for: over BLE a folder listing is fine but a file copy
/// runs at ~40 KiB/s, so a transfer that should take a second takes minutes.
/// Without this the app looks broken rather than degraded, and the fix —
/// putting both devices on the same Wi-Fi — is one the user can actually act
/// on, which is the test for whether a notification earns its place.
fn warn_user_slow_link() {
    tokio::spawn(async {
        match crate::notify::show_banner(
            "Phone files over Bluetooth",
            "Wi-Fi isn't reachable, so browsing and copying will be slow. \
             Put both devices on the same network to speed it up.",
            "vortex",
            &[],
            0,
            // Not urgent: this is a "why is it slow" explanation, not something
            // to act on before continuing. Sticking it on screen until
            // dismissed would be worse than the problem it describes.
            false,
        )
        .await
        {
            // Logged either way: the whole point is to tell the user why
            // things got slow, so a notification daemon that refused it is
            // worth knowing about rather than assuming it landed.
            Ok(_) => tracing::info!("fs-lan: told the user we are on the slow link"),
            Err(e) => tracing::warn!("fs-lan: could not show the slow-link notice: {e}"),
        }
    });
}
