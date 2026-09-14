//! Laptop-media remote layer (the laptop half of bidirectional media
//! control): ships the laptop's MPRIS now-playing snapshot to the phone on
//! the AppState heartbeat (the phone renders a MediaStyle notification with
//! transport buttons from it), and executes the phone's `media_control`
//! transport commands via MPRIS. The phone-media direction is the
//! live-activity pill (`live_activity.rs`) + `CallAction` media verbs.

use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "linux")]
use vortex_l3_daemon::core::media_runtime::session_conn;

/// Fill the laptop→phone now-playing fields on an outgoing AppState.
/// Leaves them empty (phone clears its notification) when no MPRIS player
/// has a readable title.
/// No MPRIS, no now-playing: the fields stay empty and the phone clears its
/// laptop-media notification, which is the same thing that happens on Linux
/// when nothing is playing.
#[cfg(not(target_os = "linux"))]
pub(crate) async fn fill_now_playing(_state: &mut vortex_l3_daemon::core::appstate::AppState) {}

#[cfg(target_os = "linux")]
pub(crate) async fn fill_now_playing(state: &mut vortex_l3_daemon::core::appstate::AppState) {
    let Some(c) = session_conn().await else { return };
    if let Some(np) = vortex_l3_daemon::core::media_runtime::now_playing(c).await {
        state.media_title = np.title;
        state.media_artist = np.artist;
        state.media_app = np.app;
        state.media_art_url = np.art_url;
        state.media_np_playing = np.playing;
    }
}

/// Highest `media_control_seq` already executed. BLE and LAN can both
/// deliver the same phone AppState (and the phone re-sends the snapshot
/// until the command's TTL), so execution must be edge-triggered — the
/// `dispatch_lock_command` model.
static LAST_MEDIA_SEQ: AtomicU64 = AtomicU64::new(0);

/// Execute a phone-issued media transport command carried in its AppState
/// (`media_control` + monotonic `media_control_seq`). Called from BOTH
/// state ingress paths (LAN heartbeat + BLE STATE consumer). Trust model:
/// the snapshot only ever arrives over the Noise-authenticated transport.
pub(crate) fn dispatch_media_command(state: &vortex_l3_daemon::core::appstate::AppState) {
    let cmd = state.media_control.clone();
    let seq = state.media_control_seq;
    if cmd.is_empty() || seq == 0 || seq <= LAST_MEDIA_SEQ.load(Ordering::Relaxed) {
        return;
    }
    LAST_MEDIA_SEQ.store(seq, Ordering::Relaxed);
    // The transport buttons on the phone's laptop-media notification drive
    // MPRIS. Nothing to drive elsewhere; the phone's optimistic flip simply
    // never gets confirmed, which is what it already handles for a player that
    // ignores the command.
    #[cfg(target_os = "linux")]
    tokio::spawn(async move {
        if let Some(c) = session_conn().await {
            vortex_l3_daemon::core::media_runtime::media_control(c, &cmd).await;
            // Let the player's PlaybackStatus settle, then nudge a heartbeat
            // so the phone's ⏸/▶ gets CONFIRMED in ~1s instead of riding the
            // next periodic beat (the phone flips optimistically meanwhile).
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            if let Some(n) = crate::SYNC_NUDGE.get() {
                n.notify_one();
            }
        }
    });
}
