//! Capture THIS laptop's desktop notifications and forward them to the
//! phone (the laptop→phone half of notification mirroring).
//!
//! There's no portable D-Bus API to *receive* other apps' notifications,
//! so — like the ecosystem — we eavesdrop the `Notify` method calls on the
//! session bus via a `dbus-monitor` subprocess and parse its output. zbus 5
//! can't eavesdrop method calls without becoming a full monitor connection
//! (no `MonitorBuilder` in this version), and the subprocess is the
//! battle-tested path.
//!
//! Notify signature (org.freedesktop.Notifications):
//!   Notify(app_name s, replaces_id u, app_icon s, summary s, body s,
//!          actions as, hints a{sv}, expire_timeout i)
//! so the FOUR `string` lines before the first `array [` are, in order:
//!   [0] app_name  [1] app_icon  [2] summary(title)  [3] body(text).
//!
//! **Loop guard:** we post mirrored PHONE notifications via
//! `notification_display` with app_name "Vortex"; those would be caught
//! here and bounced back to the phone forever. Skip app_name == "Vortex".

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::core::notif_mirror::NotificationMirror;

/// Collapse the same content re-emitted within this window (apps re-Notify
/// on every minor update). Matches the phone-side + ecosystem 4s.
const DEDUP_WINDOW: Duration = Duration::from_millis(4_000);
const MAX_TITLE: usize = 120;
const MAX_TEXT: usize = 280;
/// Backoff before respawning dbus-monitor if it dies / fails to start.
const RESPAWN_BACKOFF: Duration = Duration::from_secs(5);

/// Spawn the capture task. It forwards each (deduped, non-self) desktop
/// notification as a `NotificationMirror` on `tx`. Runs until the daemon
/// exits, respawning dbus-monitor if it dies.
pub fn spawn(tx: UnboundedSender<NotificationMirror>) {
    tokio::spawn(async move {
        let mut recent: HashMap<String, Instant> = HashMap::new();
        loop {
            let mut child = match tokio::process::Command::new("dbus-monitor")
                .args([
                    "--session",
                    "interface='org.freedesktop.Notifications',member='Notify'",
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    warn!("notif-capture: dbus-monitor spawn failed: {e}; laptop→phone notifications off");
                    tokio::time::sleep(RESPAWN_BACKOFF).await;
                    continue;
                }
            };
            let Some(stdout) = child.stdout.take() else {
                tokio::time::sleep(RESPAWN_BACKOFF).await;
                continue;
            };
            info!("notif-capture: watching desktop Notify calls");
            let mut lines = tokio::io::BufReader::new(stdout).lines();

            // Per-call accumulator: the leading `string` args before `array [`.
            let mut collecting = false;
            let mut strs: Vec<String> = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                let t = line.trim_start();
                if t.starts_with("method call") {
                    // New Notify call — the previous one (if any) never saw
                    // its arrays; drop it and start fresh.
                    collecting = true;
                    strs.clear();
                    continue;
                }
                if !collecting {
                    continue;
                }
                if t.starts_with("array ") {
                    // Reached the actions array — the 4 leading strings are in.
                    finalize(&strs, &tx, &mut recent);
                    collecting = false;
                    strs.clear();
                    continue;
                }
                if let Some(s) = parse_string_line(t) {
                    strs.push(s);
                }
            }
            // Stream ended (dbus-monitor died / bus restart) → respawn.
            warn!("notif-capture: dbus-monitor stream ended; respawning");
            let _ = child.kill().await;
            tokio::time::sleep(RESPAWN_BACKOFF).await;
        }
    });
}

/// Extract the inner text of a `string "..."` dbus-monitor line. Tolerant
/// of a missing trailing quote (multi-line bodies wrap; we keep the first
/// line — good enough for a notification).
fn parse_string_line(t: &str) -> Option<String> {
    let rest = t.strip_prefix("string \"")?;
    Some(rest.strip_suffix('"').unwrap_or(rest).to_string())
}

fn finalize(
    strs: &[String],
    tx: &UnboundedSender<NotificationMirror>,
    recent: &mut HashMap<String, Instant>,
) {
    if strs.len() < 4 {
        return;
    }
    let app = strs[0].trim();
    // Loop guard: skip notifications WE posted (mirrored from the phone) so
    // they don't bounce back. Our notifications now carry the real phone app
    // name, but their app_icon is ALWAYS an absolute path under the vortex
    // icon cache (a cached logo or the bundled generic) — a reliable marker.
    // Keep the legacy app_name=="Vortex" check too for older builds.
    let app_icon = strs.get(1).map(|s| s.as_str()).unwrap_or("");
    if app.eq_ignore_ascii_case("vortex") || app_icon.contains("/.cache/vortex/") {
        return;
    }
    let title = normalize(&strs[2]);
    let text = normalize(&strs[3]);
    if title.is_empty() && text.is_empty() {
        return;
    }

    let now = Instant::now();
    let key = format!("{app}|{title}|{text}");
    // Dedup + opportunistic prune of stale keys.
    recent.retain(|_, t| now.duration_since(*t) < DEDUP_WINDOW);
    if let Some(prev) = recent.get(&key) {
        if now.duration_since(*prev) < DEDUP_WINDOW {
            return;
        }
    }
    recent.insert(key, now);

    let notif = NotificationMirror {
        app: app.to_string(),
        title: title.chars().take(MAX_TITLE).collect(),
        text: text.chars().take(MAX_TEXT).collect(),
        ts: now_ms(),
        // A stable identity for this conversation, so the phone can REPLACE
        // rather than stack. Without it every update from a chat that edits its
        // own notification — the normal case — arrived on the phone as another
        // separate notification, and closing one on either device meant nothing
        // to the other. App plus title is the same pairing the dedup above
        // already treats as "the same thread".
        key: format!("laptop:{app}:{title}"),
        ..Default::default()
    };
    info!(app = %notif.app, "notif-capture: desktop notification → phone");
    let _ = tx.send(notif);
}

/// Collapse whitespace runs (incl. the literal `\n` dbus-monitor may print
/// for embedded newlines) into single spaces.
fn normalize(s: &str) -> String {
    s.replace("\\n", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
