//! MPRIS-based media pause/resume for the call-handoff path.
//!
//! When a phone call lands on the user's phone we want to:
//!   1. Pause whatever's playing on the laptop (Spotify, mpv, browser tabs, …)
//!      so the user doesn't lose their place — without this they'd
//!      come back to "wait, where was I?" silence after the call.
//!   2. Remember WHICH players were playing AND when, so when the
//!      call ends we resume *exactly those players* with a TTL
//!      guard: long calls (>5 min) skip the resume so the user
//!      doesn't get surprise music while reading email three hours
//!      later.
//!
//! Implementation: enumerates `org.mpris.MediaPlayer2.*` bus names on
//! the session bus, calls `Player.Pause` on the ones reporting
//! `PlaybackStatus == "Playing"`, stashes their names in a
//! [`MediaState::PausedForCall`] variant with a Unix timestamp, and
//! calls `Player.Play` on each one on resume (within the TTL).
//!
//! ## Why a hand-rolled state struct instead of "just track names"
//!
//! The 63eeaee prototype tracked "pending resume" through a chain of
//! callbacks. The state leaked across call cycles — a second call
//! arriving before the first had cleanly resumed would double-pause
//! a player or strand it paused forever. The explicit enum + RWMutex
//! here makes the lifecycle obvious and lets the responder enforce
//! single-call-at-a-time semantics from one place.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use zbus::zvariant::OwnedValue;
use zbus::{Connection, Proxy};

/// How long after pausing a player we are still willing to auto-resume
/// it. Long calls almost certainly mean the user has moved on to
/// something else; we'd rather they hit play manually than spook
/// them with audio.
const RESUME_TTL: Duration = Duration::from_secs(5 * 60);

/// Where the media-runtime state currently sits. Single source of
/// truth for the call-handoff resume decision.
#[derive(Debug, Clone)]
pub enum MediaState {
    /// Nothing waiting to be resumed.
    Idle,
    /// We paused these players because the user got a phone call.
    /// Resume IFF the call ends within [`RESUME_TTL`] of `paused_at`.
    PausedForCall {
        bus_names: Vec<String>,
        paused_at: SystemTime,
    },
}

impl MediaState {
    pub fn is_paused(&self) -> bool {
        matches!(self, MediaState::PausedForCall { .. })
    }

    pub fn age(&self) -> Option<Duration> {
        if let MediaState::PausedForCall { paused_at, .. } = self {
            SystemTime::now().duration_since(*paused_at).ok()
        } else {
            None
        }
    }
}

/// Shared mutable state — passed into the orchestrator by reference
/// so the same struct lives across the whole call cycle and is the
/// single place the responder reads/writes the pause record.
pub type MediaStateStore = Arc<RwLock<MediaState>>;

pub fn new_media_state_store() -> MediaStateStore {
    Arc::new(RwLock::new(MediaState::Idle))
}

/// Pause every currently-playing MPRIS player and remember them. If
/// the state was already `PausedForCall` (e.g. a second call landed
/// before the first one finished resuming), we keep the older record
/// — those players were paused FIRST and that's the lifecycle we
/// trust. Returns the list of bus names actually paused this call.
pub async fn pause_playing_for_call(store: &MediaStateStore) -> Vec<String> {
    // Bail early if we're already mid-pause — the responder protocol
    // is single-flight at the orchestrator layer, but a defensive
    // check here keeps a botched concurrent invocation from
    // double-pausing.
    {
        let s = store.read().await;
        if s.is_paused() {
            warn!("pause_playing_for_call: state already Paused; skipping");
            return Vec::new();
        }
    }

    let conn = match Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            warn!("pause: session bus unavailable: {e}");
            return Vec::new();
        }
    };

    let players = match list_mpris_players(&conn).await {
        Ok(p) => p,
        Err(e) => {
            warn!("pause: list MPRIS failed: {e}");
            return Vec::new();
        }
    };

    let mut paused: Vec<String> = Vec::new();
    for name in players {
        match player_is_playing(&conn, &name).await {
            Ok(true) => {
                if call_player_method(&conn, &name, "Pause").await.is_ok() {
                    info!(player = %name, "paused for call");
                    paused.push(name);
                }
            }
            Ok(false) => {
                debug!(player = %name, "not playing, skip");
            }
            Err(e) => {
                debug!(player = %name, "playback-status read failed: {e}");
            }
        }
    }

    if !paused.is_empty() {
        let mut s = store.write().await;
        *s = MediaState::PausedForCall {
            bus_names: paused.clone(),
            paused_at: SystemTime::now(),
        };
    }
    paused
}

/// Resume the players we paused — but only if the pause is fresh
/// (within [`RESUME_TTL`]). Stale records (long call, daemon restart
/// without R1 catching it, etc.) are cleared without playing. Returns
/// the list of bus names actually resumed.
pub async fn resume_paused_for_call(store: &MediaStateStore) -> Vec<String> {
    let (bus_names, paused_at) = {
        let s = store.read().await;
        match s.clone() {
            MediaState::PausedForCall { bus_names, paused_at } => (bus_names, paused_at),
            MediaState::Idle => return Vec::new(),
        }
    };

    let age = SystemTime::now()
        .duration_since(paused_at)
        .unwrap_or(Duration::ZERO);
    if age > RESUME_TTL {
        info!(age_s = age.as_secs(), "skip resume: pause record too old");
        let mut s = store.write().await;
        *s = MediaState::Idle;
        return Vec::new();
    }

    let conn = match Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            warn!("resume: session bus unavailable: {e}");
            return Vec::new();
        }
    };

    let mut resumed: Vec<String> = Vec::new();
    for name in &bus_names {
        if call_player_method(&conn, name, "Play").await.is_ok() {
            info!(player = %name, "resumed after call");
            resumed.push(name.clone());
        } else {
            debug!(player = %name, "resume failed (player gone?)");
        }
    }

    // 50 ms quick-retry: WirePlumber occasionally fires a route-migration
    // event between our `Play` call and the time the player actually starts,
    // and the player auto-pauses itself. Re-issuing Play after a brief
    // settle catches the most common case in one extra round trip.
    // (Pattern lifted from ecosystem commit 63eeaee — the old project's
    // "fixed with chatgpt 2" version that the user confirmed worked
    // smoothly. See its media_mpris.rs resume_if_marked Step 2.)
    tokio::time::sleep(Duration::from_millis(50)).await;
    for name in &bus_names {
        if let Ok(false) = player_is_playing(&conn, name).await {
            let _ = call_player_method(&conn, name, "Play").await;
        }
    }

    let mut s = store.write().await;
    *s = MediaState::Idle;
    drop(s);

    // Background safety-net for WirePlumber's auto-pause waves. The route
    // migration completes in stages — first the sink becomes default,
    // then existing sink-inputs are moved one by one, and each move can
    // momentarily land a stream on the old sink. Players observe this as
    // "device gone" and pause themselves. We re-issue Play at three
    // checkpoints without blocking the caller; if everything is already
    // playing after the first checkpoint we exit early.
    // (Same pattern as ecosystem 63eeaee's Step 3 background thread.)
    let bg_bus_names = bus_names.clone();
    tokio::spawn(async move {
        let bg_conn = match Connection::session().await {
            Ok(c) => c,
            Err(_) => return,
        };
        // Checkpoint *deltas* (ms to sleep before each re-check), as
        // cumulative absolute times: 40, 80, 120, 200, 500, 900, 1500,
        // 2200, 3000, 4000 ms after the initial Play.
        //
        // Dense early checkpoints (40..200ms) catch the WirePlumber
        // "device-change auto-pause" wave that fires within the first
        // ~150ms. The MIDDLE+LATE tail (900..4000ms) is the one that
        // matters on a *return* switch: the A2DP sink is created early
        // (wait_ms≈2.5s) but PipeWire then SUSPENDS and re-opens it once
        // the real A2DP codec stream negotiates — that node churn lands
        // ~1-3s after our Play and the player auto-pauses itself. The
        // old 900ms window closed before this late wave, so the buds
        // came back but playback stayed paused (user-reported). Covering
        // out to 4s tracks the sink-settle floor.
        let deltas_ms: [u64; 10] =
            [40, 40, 40, 80, 300, 400, 600, 700, 800, 1000];
        let mut elapsed_ms = 0u64;
        for delta in deltas_ms {
            tokio::time::sleep(Duration::from_millis(delta)).await;
            elapsed_ms += delta;
            let mut replayed: Vec<String> = Vec::new();
            for name in &bg_bus_names {
                if let Ok(false) = player_is_playing(&bg_conn, name).await {
                    let _ = call_player_method(&bg_conn, name, "Play").await;
                    replayed.push(name.clone());
                }
            }
            if !replayed.is_empty() {
                info!(
                    checkpoint_ms = elapsed_ms,
                    ?replayed,
                    "safety-net: player(s) had paused — re-played"
                );
            }
        }
        info!("safety-net: window closed (4000ms)");
    });

    resumed
}

/// Drop any stashed pause record without trying to resume. Used by
/// the orchestrator when an unrelated switch begins so we don't
/// re-play the user's old music on the next call-end. Cheap.
pub async fn clear(store: &MediaStateStore) {
    let mut s = store.write().await;
    *s = MediaState::Idle;
}

// ---- D-Bus helpers ----

/// Pause every currently-playing MPRIS player and return their bus
/// names. Used by the smart audio-follow watcher to silence laptop media
/// during a hand-off (so nothing blasts through the laptop speakers while
/// the buds move), symmetric to the phone's pause. Separate from the
/// call-pause [`MediaState`] store — the watcher holds the returned list
/// itself and resumes via [`play_players`].
pub(crate) async fn pause_all_playing(conn: &Connection) -> Vec<String> {
    let mut paused = Vec::new();
    if let Ok(players) = list_mpris_players(conn).await {
        for p in players {
            if player_is_playing(conn, &p).await.unwrap_or(false)
                && call_player_method(conn, &p, "Pause").await.is_ok()
            {
                paused.push(p);
            }
        }
    }
    paused
}

/// Re-play the given MPRIS players (best-effort). Companion to
/// [`pause_all_playing`].
pub(crate) async fn play_players(conn: &Connection, names: &[String]) {
    for n in names {
        let _ = call_player_method(conn, n, "Play").await;
    }
}

/// True if any MPRIS player on the session bus reports PlaybackStatus
/// "Playing" right now. Best-effort snapshot the smart audio-follow
/// watcher polls to detect local playback. Caller supplies a persistent
/// session [`Connection`] so we don't open a new bus connection every
/// poll. Returns false on any D-Bus error.
/// The bus names of every MPRIS player currently reporting Playing. The
/// smart audio-follow watcher tracks this each tick so that when the buds
/// are yanked away — and route-sensitive players (firefox) auto-pause
/// instantly, before we can — it still knows which players to resume on
/// return (from the previous tick's snapshot, not the now-empty one).
pub(crate) async fn playing_players(conn: &Connection) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(players) = list_mpris_players(conn).await {
        for p in players {
            if player_is_playing(conn, &p).await.unwrap_or(false) {
                out.push(p);
            }
        }
    }
    out
}

/// Is anything playing on this laptop right now?
///
/// Used by the proximity auto-lock: the idle gate only knows whether a key has
/// been pressed, which reads "idle" for the whole length of a film. Locking the
/// screen on someone watching something is the machine being confidently wrong
/// about where its owner is.
///
/// Uses the cached session connection, and answers `false` if the bus is
/// unreachable — which keeps the previous behaviour rather than accidentally
/// disabling auto-lock when D-Bus is unhappy.
pub async fn any_player_playing() -> bool {
    let Some(conn) = session_conn().await else {
        return false;
    };
    !playing_players(conn).await.is_empty()
}

/// Process-wide cached session-bus connection for callers outside this
/// crate (the Tauri app's media remote): repeated `Connection::session()`
/// leaks D-Bus connections — the per-tick `bluer::Session::new` trap.
static SESSION_CONN: tokio::sync::OnceCell<Connection> = tokio::sync::OnceCell::const_new();

pub async fn session_conn() -> Option<&'static Connection> {
    match SESSION_CONN.get_or_try_init(Connection::session).await {
        Ok(c) => Some(c),
        Err(e) => {
            warn!("session bus unavailable for media remote: {e}");
            None
        }
    }
}

/// Now-playing snapshot of the laptop's media, shipped to the phone in the
/// AppState heartbeat (the phone renders a MediaStyle notification with
/// transport buttons from it).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    /// The player's human name (MPRIS `Identity`, e.g. "Spotify").
    pub app: String,
    /// http(s) cover art the PHONE fetches for its media card. See
    /// [`art_url_of`] for where it comes from; empty when there is none.
    pub art_url: String,
    pub playing: bool,
}

/// Read the laptop's now-playing snapshot: the first PLAYING player, else the
/// first player with a readable title (a paused player still shows on the
/// phone so the track can be resumed from there). None = no player / no
/// metadata worth showing.
pub async fn now_playing(conn: &Connection) -> Option<NowPlaying> {
    let players = list_mpris_players(conn).await.ok()?;
    let mut best: Option<NowPlaying> = None;
    for p in &players {
        let playing = player_is_playing(conn, p).await.unwrap_or(false);
        if !playing && best.is_some() {
            continue; // already have a paused candidate; only playing beats it
        }
        let (title, artist, art_url) = player_metadata(conn, p).await;
        if title.is_empty() {
            continue;
        }
        let np = NowPlaying {
            title,
            artist,
            app: player_identity(conn, p).await,
            art_url,
            playing,
        };
        if playing {
            return Some(np);
        }
        best = Some(np);
    }
    best
}

/// `xesam:title` + first `xesam:artist` + cover art off the player's
/// `Metadata` property. Empty strings on any error — the caller treats
/// no-title as "nothing to show".
async fn player_metadata(conn: &Connection, bus_name: &str) -> (String, String, String) {
    const NONE: (String, String, String) = (String::new(), String::new(), String::new());
    let Ok(proxy) = Proxy::new(
        conn,
        bus_name,
        "/org/mpris/MediaPlayer2",
        "org.freedesktop.DBus.Properties",
    )
    .await
    else {
        return NONE;
    };
    let Ok(value) = proxy
        .call::<_, _, OwnedValue>("Get", &("org.mpris.MediaPlayer2.Player", "Metadata"))
        .await
    else {
        return NONE;
    };
    let Ok(dict) = <std::collections::HashMap<String, OwnedValue>>::try_from(value) else {
        return NONE;
    };
    let str_of = |key: &str| -> String {
        dict.get(key)
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            .unwrap_or_default()
    };
    let title = str_of("xesam:title");
    // `xesam:artist` is `as` (array of strings) per the MPRIS spec.
    let artist = dict
        .get("xesam:artist")
        .and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok())
        .and_then(|a| a.into_iter().find(|s| !s.is_empty()))
        .unwrap_or_default();
    let art = art_url_of(&str_of("mpris:artUrl"), &str_of("xesam:url"));
    (title.trim().to_string(), artist.trim().to_string(), art)
}

/// Pick the cover art the phone can actually fetch.
///
/// `mpris:artUrl` is the spec answer and Spotify/Chromium publish a usable
/// https one. It is skipped when it points at a `file://` path or a `data:`
/// blob: this URL travels to the PHONE, which can neither read the laptop's
/// disk nor be handed a megabyte of base64 on the heartbeat.
///
/// Firefox publishes NO artUrl at all for a YouTube tab (live-checked
/// 2026-08-10: its Metadata carries only trackid/title/album/artist/url/
/// length), so fall back to deriving the video's own thumbnail from
/// `xesam:url` — that's what makes a YouTube tab look like the phone's own
/// YouTube media card instead of a blank square.
fn art_url_of(art_url: &str, page_url: &str) -> String {
    if art_url.starts_with("http://") || art_url.starts_with("https://") {
        return art_url.to_string();
    }
    youtube_thumb(page_url).unwrap_or_default()
}

/// `https://i.ytimg.com/vi/<id>/hqdefault.jpg` for a YouTube watch / shorts /
/// youtu.be link. None for anything else.
fn youtube_thumb(page_url: &str) -> Option<String> {
    let rest = page_url
        .strip_prefix("https://")
        .or_else(|| page_url.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/')?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    let id = match host {
        "youtu.be" => path.split(['?', '&', '/']).next()?,
        "youtube.com" | "m.youtube.com" | "music.youtube.com" => {
            if let Some(short) = path.strip_prefix("shorts/") {
                short.split(['?', '&', '/']).next()?
            } else {
                // watch?v=<id>&… — the v param is not always first.
                path.split_once('?')?
                    .1
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("v="))?
            }
        }
        _ => return None,
    };
    // A YouTube id is 11 url-safe chars; anything else means we mis-parsed and
    // would hand the phone a 404.
    if id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        Some(format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"))
    } else {
        None
    }
}

/// The player's human-readable name (MPRIS `Identity` on the base
/// interface), falling back to the bus-name suffix ("spotify").
async fn player_identity(conn: &Connection, bus_name: &str) -> String {
    let ident = async {
        let proxy = Proxy::new(
            conn,
            bus_name,
            "/org/mpris/MediaPlayer2",
            "org.freedesktop.DBus.Properties",
        )
        .await
        .ok()?;
        let value = proxy
            .call::<_, _, OwnedValue>("Get", &("org.mpris.MediaPlayer2", "Identity"))
            .await
            .ok()?;
        String::try_from(value).ok().filter(|s| !s.is_empty())
    }
    .await;
    ident.unwrap_or_else(|| {
        bus_name
            .strip_prefix("org.mpris.MediaPlayer2.")
            .unwrap_or(bus_name)
            .split('.')
            .next()
            .unwrap_or_default()
            .to_string()
    })
}

/// Execute a phone-driven transport command on the laptop's media:
/// `media_play_pause` / `media_next` / `media_prev`. Targets the first
/// PLAYING player, else the first player at all (so play-from-paused works).
pub async fn media_control(conn: &Connection, cmd: &str) {
    let method: &'static str = match cmd {
        "media_play_pause" => "PlayPause",
        "media_next" => "Next",
        "media_prev" => "Previous",
        _ => {
            warn!(cmd, "unknown media-control command; ignoring");
            return;
        }
    };
    let Ok(players) = list_mpris_players(conn).await else {
        return;
    };
    let mut target: Option<String> = players.first().cloned();
    for p in &players {
        if player_is_playing(conn, p).await.unwrap_or(false) {
            target = Some(p.clone());
            break;
        }
    }
    let Some(t) = target else {
        info!(cmd, "media-control but no MPRIS player is up");
        return;
    };
    match call_player_method(conn, &t, method).await {
        Ok(()) => info!(cmd, player = %t, "media-control executed"),
        Err(e) => warn!(cmd, player = %t, "media-control failed: {e}"),
    }
}

/// Returns every well-known name on the session bus that starts with
/// `org.mpris.MediaPlayer2.` — that's the standard MPRIS convention.
pub(crate) async fn list_mpris_players(conn: &Connection) -> zbus::Result<Vec<String>> {
    let proxy = Proxy::new(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await?;
    let names: Vec<String> = proxy.call("ListNames", &()).await?;
    Ok(names
        .into_iter()
        .filter(|n| n.starts_with("org.mpris.MediaPlayer2.") && !is_mpris_proxy(n))
        .collect())
}

/// True for MPRIS buses that are a PROXY of another player rather than a
/// player of their own.
///
/// `playerctld` re-publishes whatever is currently active — same metadata,
/// same `Identity` — so the same Firefox tab appears on the bus twice. Left
/// in, the call-handoff path pauses that one player through both names and
/// resumes it twice, and `now_playing` can pick the proxy (whose properties
/// silently swap to a DIFFERENT player the moment the user starts one).
fn is_mpris_proxy(bus_name: &str) -> bool {
    bus_name == "org.mpris.MediaPlayer2.playerctld"
}

pub(crate) async fn player_is_playing(conn: &Connection, bus_name: &str) -> zbus::Result<bool> {
    let proxy = Proxy::new(
        conn,
        bus_name,
        "/org/mpris/MediaPlayer2",
        "org.freedesktop.DBus.Properties",
    )
    .await?;
    let value: OwnedValue = proxy
        .call(
            "Get",
            &("org.mpris.MediaPlayer2.Player", "PlaybackStatus"),
        )
        .await?;
    // The PlaybackStatus property is a variant carrying a string.
    // We accept it via `TryInto<String>` so any unusual encoding
    // (newer zbus internals, server-side weirdness) just looks
    // like "not Playing" instead of panicking.
    let s: String = String::try_from(value).unwrap_or_default();
    Ok(s == "Playing")
}

async fn call_player_method(
    conn: &Connection,
    bus_name: &str,
    method: &'static str,
) -> zbus::Result<()> {
    let proxy = Proxy::new(
        conn,
        bus_name,
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2.Player",
    )
    .await?;
    proxy.call::<_, _, ()>(method, &()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn art_url_prefers_a_real_http_art_url() {
        let art = "https://i.scdn.co/image/abc";
        assert_eq!(art_url_of(art, "https://youtu.be/aaaaaaaaaaa"), art);
    }

    #[test]
    fn art_url_rejects_art_the_phone_cannot_fetch() {
        // file:// lives on the LAPTOP's disk; data: would be megabytes of
        // base64 on the heartbeat. Both fall through to the page URL.
        assert_eq!(art_url_of("file:///tmp/cover.png", ""), "");
        assert_eq!(art_url_of("data:image/png;base64,iVBOR", ""), "");
    }

    #[test]
    fn youtube_thumb_covers_the_shapes_a_browser_publishes() {
        let want = Some("https://i.ytimg.com/vi/u3SgSdVOFQw/hqdefault.jpg".to_string());
        // The exact URL Firefox published in the live check.
        assert_eq!(youtube_thumb("https://www.youtube.com/watch?v=u3SgSdVOFQw"), want);
        assert_eq!(youtube_thumb("https://m.youtube.com/watch?t=42&v=u3SgSdVOFQw"), want);
        assert_eq!(youtube_thumb("https://youtu.be/u3SgSdVOFQw?t=42"), want);
        assert_eq!(youtube_thumb("https://www.youtube.com/shorts/u3SgSdVOFQw"), want);
    }

    #[test]
    fn youtube_thumb_declines_what_it_did_not_parse() {
        assert_eq!(youtube_thumb("https://vimeo.com/watch?v=u3SgSdVOFQw"), None);
        assert_eq!(youtube_thumb("https://www.youtube.com/feed/subscriptions"), None);
        // A short/long id means we mis-parsed — better no art than a 404.
        assert_eq!(youtube_thumb("https://www.youtube.com/watch?v=abc"), None);
        assert_eq!(youtube_thumb("file:///home/u/song.mp3"), None);
    }

    #[tokio::test]
    async fn idle_initial_state() {
        let store = new_media_state_store();
        let s = store.read().await;
        assert!(matches!(*s, MediaState::Idle));
        assert!(!s.is_paused());
        assert!(s.age().is_none());
    }

    #[tokio::test]
    async fn paused_state_age_grows() {
        let store = new_media_state_store();
        {
            let mut s = store.write().await;
            *s = MediaState::PausedForCall {
                bus_names: vec!["org.mpris.MediaPlayer2.spotify".into()],
                paused_at: SystemTime::now() - Duration::from_secs(2),
            };
        }
        let s = store.read().await;
        assert!(s.is_paused());
        let age = s.age().expect("paused state must have age");
        assert!(age >= Duration::from_secs(2));
    }

    #[tokio::test]
    async fn clear_drops_pause_record() {
        let store = new_media_state_store();
        {
            let mut s = store.write().await;
            *s = MediaState::PausedForCall {
                bus_names: vec!["a".into()],
                paused_at: SystemTime::now(),
            };
        }
        clear(&store).await;
        let s = store.read().await;
        assert!(matches!(*s, MediaState::Idle));
    }

    #[tokio::test]
    async fn resume_skips_stale() {
        // Past the 5-min TTL → resume should clear the slot and
        // return an empty list, no D-Bus traffic.
        let store = new_media_state_store();
        {
            let mut s = store.write().await;
            *s = MediaState::PausedForCall {
                bus_names: vec!["org.mpris.MediaPlayer2.imaginary".into()],
                paused_at: SystemTime::now() - Duration::from_secs(6 * 60),
            };
        }
        let resumed = resume_paused_for_call(&store).await;
        assert!(resumed.is_empty());
        let s = store.read().await;
        assert!(matches!(*s, MediaState::Idle));
    }
}
