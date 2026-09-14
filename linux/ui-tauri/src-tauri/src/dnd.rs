//! Do Not Disturb, shared with the phone.
//!
//! The setting the two devices disagreeing on is the most annoying one to get
//! wrong: you silence the laptop for a meeting and the phone buzzes on the
//! table anyway. So it is synced, last-writer-wins, exactly like the
//! smart-switch setting in `media_watch.rs` — see `AppState::dnd` for the wire
//! contract and for why only an explicit toggle is ever propagated.
//!
//! On the desktop side DND is not one thing. GNOME has
//! `org.gnome.desktop.notifications show-banners`, which is what its own
//! "Do Not Disturb" switch writes. KDE and anything else implementing the
//! freedesktop spec expose an `Inhibited` property on
//! `org.freedesktop.Notifications`. We read and write whichever is present,
//! and report honestly when neither is.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Our view of the shared setting, and when it was last explicitly changed.
static DND: AtomicBool = AtomicBool::new(false);
static DND_CHANGED_AT: AtomicU64 = AtomicU64::new(0);

/// Where the shared setting is persisted, so a restart RESUMES it.
///
/// Without this the laptop starts at `changed_at = 0` while the phone still
/// holds its last stamp, so the first inbound snapshot always wins and the
/// laptop silently adopts an opinion that may be hours old. Measured: the
/// desktop was in Do Not Disturb, the app restarted, and 0.2 s later the log
/// read "adopted the phone's setting on=false" — DND switched off with nobody
/// touching anything. The smart-switch setting avoids exactly this by
/// persisting its pair; this one copied the wire contract but not the storage.
fn store_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("vortex").join("dnd.json"))
}

fn load_persisted() -> Option<(bool, u64)> {
    let raw = std::fs::read_to_string(store_path()?).ok()?;
    let on = raw.contains("\"on\": true");
    let at = raw
        .split("\"changed_at\":")
        .nth(1)?
        .trim()
        .trim_end_matches('}')
        .trim()
        .parse::<u64>()
        .ok()?;
    Some((on, at))
}

fn save_persisted(on: bool, at: u64) {
    let Some(path) = store_path() else { return };
    let body = format!("{{\n  \"on\": {on},\n  \"changed_at\": {at}\n}}\n");
    let _ = vortex_l3_daemon::core::fs_private::write_private(&path, body.as_bytes());
}

pub fn state() -> (bool, u64) {
    (
        DND.load(Ordering::Relaxed),
        DND_CHANGED_AT.load(Ordering::Relaxed),
    )
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn gsettings(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("gsettings").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Read the desktop's own DND state. `None` = this desktop exposes neither
/// mechanism, so there is nothing to mirror and nothing to claim.
pub fn read_desktop() -> Option<bool> {
    if let Some(v) = gsettings(&["get", "org.gnome.desktop.notifications", "show-banners"]) {
        // show-banners is the INVERSE of Do Not Disturb.
        return Some(v == "false");
    }
    None
}

/// Apply DND to this desktop. Best-effort and idempotent.
///
/// Logged on every call, deliberately. The setting has been observed reverting
/// on its own a few seconds after being set, and the first thing to establish
/// is whether WE wrote it back — a log line here answers that from the next
/// occurrence instead of another round of reasoning about the state machine.
fn write_desktop(on: bool) -> bool {
    let want = if on { "false" } else { "true" };
    tracing::info!(on, "dnd: writing the desktop setting");
    gsettings(&[
        "set",
        "org.gnome.desktop.notifications",
        "show-banners",
        want,
    ])
    .is_some()
}

/// The user changed DND ON THIS LAPTOP: adopt it and stamp the clock so the
/// phone adopts it too. Called by the watcher below, never by an inbound
/// snapshot.
fn note_local_change(on: bool) {
    DND.store(on, Ordering::Relaxed);
    // Monotonic: never mint a stamp that our own last one already matches or
    // beats. Wall clocks between the two devices agree closely but not
    // exactly, and a toggle made a moment after adopting the peer's value
    // would otherwise carry a stamp the peer discards — the user's click
    // silently lost. The phone's smart switch guards its stamp the same way.
    let at = now_secs().max(DND_CHANGED_AT.load(Ordering::Relaxed) + 1);
    DND_CHANGED_AT.store(at, Ordering::Relaxed);
    save_persisted(on, at);
    // Send it NOW, not on the next beat.
    //
    // The phone already pushes its own DND change the moment it happens
    // (`DndSync.noteLocalChange` → `pushStateViaBle`), which is why phone →
    // laptop felt instant while laptop → phone took up to a heartbeat. This is
    // the same early-wake the lock-screen hint uses, for the same reason: a
    // setting the user just flipped should not wait out a twelve-second timer.
    // BOTH transports. The BLE state loop stops after repeated write failures
    // and the LAN heartbeat stretches to minutes while BLE looks healthy, so
    // nudging only one can leave a toggle waiting a long time on a wedged
    // link. The lock-screen hint nudges both for the same reason.
    crate::presence::state_nudge().notify_one();
    if let Some(n) = crate::SYNC_NUDGE.get() {
        n.notify_one();
    }
    tracing::info!(on, at, "dnd: changed here — pushing now");
}

/// Apply the peer's DND if their toggle is newer than ours.
///
/// Mirrors `MediaWatch::apply_setting`: a strictly-greater timestamp wins, and
/// `changed_at == 0` ("no opinion") never does — which is what stops two
/// untouched defaults from re-adopting each other on every heartbeat.
pub fn apply_peer(on: bool, changed_at: u64) {
    let ours = DND_CHANGED_AT.load(Ordering::Relaxed);
    if changed_at == 0 || changed_at <= ours {
        return;
    }
    tracing::info!(
        peer_on = on,
        peer_at = changed_at,
        ours_at = ours,
        "dnd: peer stamp is newer — adopting"
    );
    DND_CHANGED_AT.store(changed_at, Ordering::Relaxed);
    save_persisted(on, changed_at);
    if DND.swap(on, Ordering::Relaxed) == on && read_desktop() == Some(on) {
        return; // already there — don't touch the desktop for nothing
    }
    if write_desktop(on) {
        tracing::info!(on, "dnd: adopted the phone's setting");
    } else {
        tracing::warn!(on, "dnd: could not apply to this desktop");
    }
}

/// Watch the desktop's own DND switch so flipping it in GNOME's menu reaches
/// the phone.
///
/// Event-driven, not a poll. `gsettings monitor` prints a line the instant the
/// key changes, so the user's click is noticed in milliseconds rather than in
/// up to a poll interval — which, once the SEND became immediate, was the
/// whole of the remaining delay. This is the same reason battery changes feel
/// instant: nothing asks, the system says.
///
/// A slow reconcile still runs underneath. The monitor is a subprocess and can
/// die, and a missed edge would otherwise leave the two devices disagreeing
/// until someone touched the switch again — the one failure this feature
/// exists to prevent.
pub fn spawn_watcher() {
    let Some(initial) = read_desktop() else {
        tracing::info!("dnd: this desktop exposes no DND setting — sync disabled");
        return;
    };
    // Resume from disk rather than starting with "no opinion", which any peer
    // stamp would beat. If we DO have a saved opinion and the desktop no
    // longer matches it, the desktop was changed while we were not running:
    // that is a local edge, and it must carry a fresh stamp so it wins.
    match load_persisted() {
        Some((saved_on, saved_at)) if saved_at > 0 => {
            DND_CHANGED_AT.store(saved_at, Ordering::Relaxed);
            DND.store(saved_on, Ordering::Relaxed);
            tracing::info!(saved_on, saved_at, initial, "dnd: resumed the saved setting");
            if initial != saved_on {
                note_local_change(initial);
            }
        }
        _ => {
            DND.store(initial, Ordering::Relaxed);
            tracing::info!(initial, "dnd: watching the desktop setting");
        }
    }
    std::thread::spawn(monitor_loop);
    std::thread::spawn(reconcile_loop);
}

/// Follow `gsettings monitor`, restarting it if it ever exits.
fn monitor_loop() {
    use std::io::BufRead;
    loop {
        let child = std::process::Command::new("gsettings")
            .args(["monitor", "org.gnome.desktop.notifications", "show-banners"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            tracing::warn!("dnd: could not start `gsettings monitor` — polling only");
            return;
        };
        if let Some(out) = child.stdout.take() {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                // "show-banners: false"
                let Some((_, v)) = line.split_once(':') else { continue };
                let banners = v.trim() == "true";
                let now = !banners; // show-banners is the inverse of DND
                if now != DND.load(Ordering::Relaxed) {
                    note_local_change(now);
                }
            }
        }
        let _ = child.wait();
        // Exited — GNOME restarted, or the session went away. Try again, but
        // not in a tight loop.
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

/// Catch anything the monitor missed. Slow on purpose: this is a backstop, not
/// the mechanism.
fn reconcile_loop() {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        let Some(now) = read_desktop() else { continue };
        if now != DND.load(Ordering::Relaxed) {
            tracing::info!(now, "dnd: reconcile found a change the monitor missed");
            note_local_change(now);
        }
    }
}
