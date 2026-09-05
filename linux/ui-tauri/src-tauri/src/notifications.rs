//! Notification-mirror Tauri commands (per-side show/send toggles) + the
//! GNOME reply-text prompt (zenity). Split out of lib.rs.


use std::sync::Arc;

/// The chat currently open AND focused in the UI (its display name, as a
/// mirrored notification's title would carry it). The notification consumer
/// skips desktop popups for this sender — the user is already reading that
/// thread. Empty = no chat open.
pub(crate) static ACTIVE_CHAT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// SMS / messaging apps whose mirrored-notification CLICK opens the conversation
/// in the laptop's Messages page. Single source of truth for "this click is
/// actionable on the laptop" (the laptop only mirrors SMS threads + the call
/// log, so only these app types have somewhere to open).
const SMS_APP_IDS: &[&str] = &[
    "com.google.android.apps.messaging", // Google Messages
    "com.android.messaging",             // AOSP Messaging
    "com.android.mms",                   // AOSP / older MIUI
    "com.samsung.android.messaging",     // Samsung
    "com.xiaomi.mms",                    // Xiaomi
    "com.miui.smsextra",                 // MIUI extras
];
/// Dialer / call apps whose mirrored notification (missed call, voicemail) opens
/// the laptop's Recents page on click.
const CALL_APP_IDS: &[&str] = &[
    "com.google.android.dialer",
    "com.android.dialer",
    "com.android.incallui",
    "com.samsung.android.incallui",
    "com.android.server.telecom",
];

/// What a mirrored-notification body-click should OPEN on the laptop, by source
/// app. `None` = informational app (Telegram, email, …) the laptop can't open →
/// the click just dismisses the notification (and mirrors the clear to the
/// phone) without hijacking focus.
fn notif_click_target(app_id: &str) -> Option<&'static str> {
    if SMS_APP_IDS.contains(&app_id) {
        Some("sms")
    } else if CALL_APP_IDS.contains(&app_id) {
        Some("call")
    } else {
        None
    }
}

/// WhatsApp packages (consumer + business) — their notification click resolves
/// the sender name to a number for a `wa.me` deep link.
fn is_whatsapp(app_id: &str) -> bool {
    matches!(app_id, "com.whatsapp" | "com.whatsapp.w4b")
}

/// Known webmail providers → their INBOX url. Email carries no per-message deep
/// link (only sender + subject), so a click opens the inbox in the default
/// browser. Kept short + explicit: unlike the open-ended chat-app space, the big
/// webmail providers are few and stable.
fn webmail_inbox(app_id: &str) -> Option<&'static str> {
    Some(match app_id {
        "com.google.android.gm" => "https://mail.google.com/",
        "com.microsoft.office.outlook" => "https://outlook.live.com/mail/",
        "com.yahoo.mobile.client.android.mail" => "https://mail.yahoo.com/",
        "ch.protonmail.android" | "me.proton.android.mail" => "https://mail.proton.me/",
        _ => return None,
    })
}

/// Reduce a stored phone number to the bare international digits `wa.me` wants
/// (drop spaces/dashes/parens/leading +). Best-effort: a number saved WITHOUT a
/// country code (local "0…" form) can't be repaired here and may not resolve.
fn wa_number(raw: &str) -> String {
    raw.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// What a mirrored-notification body-click should do on the laptop.
enum ClickAction {
    /// SMS / call → pop the Vortex window and open its page (existing path).
    Page(&'static str),
    /// Open a URL in the default browser (WhatsApp wa.me, webmail inbox).
    OpenUrl(String),
    /// Launch an installed desktop app by its `.desktop` path (Level A).
    LaunchApp(std::path::PathBuf),
    /// Nothing the laptop can open → let it just dismiss (no focus hijack).
    Dismiss,
}

/// Resolve a click, generic→specific: SMS/call laptop pages → WhatsApp deep
/// link → webmail inbox → match-and-launch ANY installed desktop app whose name
/// matches the source app (Telegram/Slack/Signal/…) → dismiss. The launch leg is
/// the dynamic core — no per-app hardcoding, driven by the notification's label.
fn resolve_notif_click(app_id: &str, app_label: &str, title: &str) -> ClickAction {
    if let Some(kind) = notif_click_target(app_id) {
        return ClickAction::Page(kind);
    }
    if is_whatsapp(app_id) {
        // Exact chat when the sender resolves to a number; else fall through and
        // just launch the WhatsApp app below.
        if let Some(num) = crate::contacts::lookup_number_by_name(title) {
            let n = wa_number(&num);
            if !n.is_empty() {
                return ClickAction::OpenUrl(format!("https://wa.me/{n}"));
            }
        }
    }
    if let Some(url) = webmail_inbox(app_id) {
        return ClickAction::OpenUrl(url.to_string());
    }
    if let Some(path) = crate::desktop_apps::match_label(app_label) {
        return ClickAction::LaunchApp(path);
    }
    ClickAction::Dismiss
}

#[cfg(test)]
mod tests {
    use super::{is_whatsapp, notif_click_target, wa_number, webmail_inbox};

    #[test]
    fn click_target_gates_by_app() {
        // SMS apps open the conversation.
        assert_eq!(notif_click_target("com.google.android.apps.messaging"), Some("sms"));
        assert_eq!(notif_click_target("com.android.messaging"), Some("sms"));
        // Dialer / telecom open the call log.
        assert_eq!(notif_click_target("com.google.android.dialer"), Some("call"));
        assert_eq!(notif_click_target("com.android.server.telecom"), Some("call"));
        // Everything else is informational → click just dismisses (no open).
        assert_eq!(notif_click_target("org.telegram.messenger"), None);
        assert_eq!(notif_click_target("com.whatsapp"), None);
        assert_eq!(notif_click_target(""), None);
    }

    #[test]
    fn whatsapp_and_webmail_routing() {
        assert!(is_whatsapp("com.whatsapp"));
        assert!(is_whatsapp("com.whatsapp.w4b"));
        assert!(!is_whatsapp("org.telegram.messenger"));
        assert_eq!(webmail_inbox("com.google.android.gm"), Some("https://mail.google.com/"));
        assert_eq!(webmail_inbox("me.proton.android.mail"), Some("https://mail.proton.me/"));
        // Telegram is not webmail → falls through to app-launch, not a browser.
        assert_eq!(webmail_inbox("org.telegram.messenger"), None);
    }

    #[test]
    fn wa_number_strips_to_digits() {
        assert_eq!(wa_number("+998 90 123-45-67"), "998901234567");
        assert_eq!(wa_number("(555) 010"), "555010");
    }
}

/// UI → backend: the open chat changed (or closed → empty name).
#[tauri::command]
pub fn set_active_chat(name: String) {
    if let Ok(mut g) = ACTIVE_CHAT.lock() {
        *g = name;
    }
}

/// Whether the laptop DISPLAYS mirrored phone notifications. Per-device,
/// LOCAL-only (NOT synced with the phone — each side controls what it
/// shows independently). Default true. The Settings toggle flips it; the
/// BLE notification consumer checks it before popping a desktop notice.
pub(crate) static NOTIF_SHOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Whether the laptop SENDS its own desktop notifications to the phone
/// (laptop→phone direction). Per-device, LOCAL-only. Default true. The
/// capture→BLE consumer checks it before writing.
pub(crate) static NOTIF_SEND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// A registered BLE notification writer for the live peer connection,
/// capturing its client + transport. Set by the persistent loop on connect,
/// cleared on disconnect; the capture consumer calls it to push a desktop
/// notification to the phone.
pub(crate) type NotifWriter = Arc<
    dyn Fn(vortex_l3_daemon::core::notif_mirror::NotificationMirror)
            -> futures::future::BoxFuture<'static, Result<(), String>>
        + Send
        + Sync,
>;

/// Monotonic id for a laptop→phone action INVOKE. Stamped on each invoke so the
/// phone dedups the SAME invoke arriving over BOTH the BLE fast-path and the
/// LAN backstop below. 0 means "not an invoke".
pub(crate) static NOTIF_INVOKE_SEQ: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The next invoke id — monotonic ACROSS RESTARTS, which is the whole point.
///
/// This used to start at 1 every launch while the phone keeps its high-water
/// mark for the life of its own process and drops anything `<=` it. So after
/// five actions, restarting the laptop app (an update, a crash, a re-login)
/// made every button and every typed reply silently do nothing: the phone
/// discarded them as already-seen, with no log and no error, and GNOME had
/// already closed the notification. It stayed dead until the laptop's counter
/// climbed past the phone's mark or the phone's process died.
///
/// Seeding from the wall clock fixes it without a state file: unix
/// milliseconds only ever go up, so a fresh process always starts above
/// anything the previous one sent. The counter then increments normally within
/// the run, so ids stay unique even if two invokes land in the same
/// millisecond.
fn next_invoke_seq() -> u64 {
    use std::sync::atomic::Ordering;
    let cur = NOTIF_INVOKE_SEQ.load(Ordering::SeqCst);
    if cur == 0 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(1);
        // Only the first caller seeds; a racing caller just falls through to
        // the fetch_add below and still gets a unique, higher id.
        let _ = NOTIF_INVOKE_SEQ.compare_exchange(0, now, Ordering::SeqCst, Ordering::SeqCst);
    }
    NOTIF_INVOKE_SEQ.fetch_add(1, Ordering::SeqCst)
}

/// LAN backstop for a notification action/reply invoke: the next outgoing
/// AppState carries this so the invoke reaches the phone even when the BLE
/// NOTIFICATION write was accepted-but-dropped (or the BLE link is wedged). The
/// phone dedups by `seq`, mirroring `PENDING_CALL_CONTROL`.
pub(crate) static PENDING_NOTIF_INVOKE: std::sync::Mutex<
    Option<vortex_l3_daemon::core::notif_mirror::NotificationMirror>,
> = std::sync::Mutex::new(None);

/// Pop a text-entry dialog so the user can type a reply to a mirrored phone
/// notification. The desktop has no inline-reply notification capability, so the
/// reply action's button can't carry a text field — we collect the text
/// out-of-band here and hand it to the phone, which fills the action's
/// RemoteInput. Returns the typed text on OK, or None when the user cancels (or
/// no dialog tool is installed) — in which case nothing fires.
///
/// Toolkit-agnostic: prefers `zenity` (GTK/GNOME) and falls back to `kdialog`
/// (Qt/KDE) so the feature also works on a Plasma desktop, where zenity is
/// usually absent. Both print the entry to stdout and exit non-zero on Cancel.
pub(crate) async fn prompt_reply_text(prompt: &str) -> Option<String> {
    // (binary, args) for each known dialog tool, in preference order. We probe
    // by actually trying to spawn — `ErrorKind::NotFound` means "not installed",
    // so we move on to the next without surfacing a failure to the user.
    let title = "Vortex — Reply";
    let candidates: [(&str, Vec<String>); 2] = [
        (
            "zenity",
            vec![
                "--entry".into(),
                format!("--title={title}"),
                format!("--text={prompt}"),
            ],
        ),
        (
            "kdialog",
            vec![
                "--title".into(),
                title.into(),
                "--inputbox".into(),
                prompt.into(),
            ],
        ),
    ];
    for (bin, args) in candidates {
        match tokio::process::Command::new(bin).args(&args).output().await {
            Ok(out) => {
                if !out.status.success() {
                    return None; // user pressed Cancel / closed the dialog
                }
                let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                return if text.is_empty() { None } else { Some(text) };
            }
            // Tool not on this box — try the next toolkit's dialog.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            // Spawned but failed for another reason — don't keep trying, give up.
            Err(_) => return None,
        }
    }
    None
}

/// Settings toggle: whether THIS laptop shows mirrored phone notifications.
/// Per-device + LOCAL — not synced to the phone (each side decides what it
/// displays). Off = incoming notification frames are dropped silently.
#[tauri::command]
pub fn set_notif_mirror_show(show: bool) {
    NOTIF_SHOW.store(show, std::sync::atomic::Ordering::Relaxed);
}

/// Current laptop notification-display state.
#[tauri::command]
pub fn get_notif_mirror_show() -> bool {
    NOTIF_SHOW.load(std::sync::atomic::Ordering::Relaxed)
}

/// Settings toggle: whether THIS laptop SENDS its own desktop notifications
/// to the phone (laptop→phone). Per-device + LOCAL.
#[tauri::command]
pub fn set_notif_mirror_send(send: bool) {
    NOTIF_SEND.store(send, std::sync::atomic::Ordering::Relaxed);
}

/// Current laptop notification-send state.
#[tauri::command]
pub fn get_notif_mirror_send() -> bool {
    NOTIF_SEND.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn spawn_icon_consumer(
) -> tokio::sync::mpsc::UnboundedSender<(String, u16, u16, Vec<u8>)> {
            let (ble_icon_tx, mut ble_icon_rx) =
                tokio::sync::mpsc::unbounded_channel::<(String, u16, u16, Vec<u8>)>();
            {
                tokio::spawn(async move {
                    let mut asm = vortex_l3_daemon::core::icon_cache::IconAssembler::default();
                    while let Some((app_id, total, idx, data)) = ble_icon_rx.recv().await {
                        if let Some(path) = asm.add(app_id.clone(), total, idx, data) {
                            tracing::info!(app_id = %app_id, path = ?path, "icon: cached app logo");
                        }
                    }
                });
            }
    ble_icon_tx
}


pub(crate) fn spawn_subsystem(
    app: tauri::AppHandle,
) -> (
    tokio::sync::mpsc::UnboundedSender<vortex_l3_daemon::core::notif_mirror::NotificationMirror>,
    std::sync::Arc<tokio::sync::Mutex<Option<crate::NotifWriter>>>,
) {
            let (ble_notif_tx, mut ble_notif_rx) = tokio::sync::mpsc::unbounded_channel::<
                vortex_l3_daemon::core::notif_mirror::NotificationMirror,
            >();
            // Dismissal-sync + reply link map: our desktop notification id →
            // (phone notification key, reply-action index). Lets us (a) close our
            // copy when the phone dismisses, (b) tell the phone when the user
            // dismisses our copy, and (c) know which action button (if any) needs
            // a typed reply so we can pop a text entry for it.
            // value = (phone key, reply-action index, shown-at, title, app_id,
            // app_label) — title + app_id/app_label drive the click resolution
            // (SMS page, WhatsApp deep link, webmail, or launch a matching
            // desktop app by name); app_label is what we match `.desktop` files
            // against, so e.g. phone "Telegram" opens Telegram Desktop.
            let notif_links: Arc<
                tokio::sync::Mutex<
                    std::collections::HashMap<
                        u32,
                        (String, i32, std::time::Instant, String, String, String),
                    >,
                >,
            > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
            // Ids that just had an action invoked. Clicking an action button on
            // GNOME emits BOTH ActionInvoked AND NotificationClosed(reason=2) for
            // the same id (the action closes the notification), and the two race.
            // We record action ids here so the racing close is recognised as the
            // action's side-effect — NOT an independent user dismissal — and is
            // therefore not mirrored to the phone as a dismiss.
            let notif_recent_actions: Arc<tokio::sync::Mutex<std::collections::HashSet<u32>>> =
                Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));

            // Published by the persistent loop on connect / cleared on disconnect.
            // Created HERE (before the show consumer) because the show consumer must
            // reach it too: when a BLE frame is dropped (or we reconnect) the daemon
            // nudges us with a `resync` marker, and we answer with a catch-up request
            // written back over this same handle.
            let ble_notif_writer: Arc<tokio::sync::Mutex<Option<NotifWriter>>> =
                Arc::new(tokio::sync::Mutex::new(None));
            // Notification keys we've actually DISPLAYED this session. Sent upstream
            // in a catch-up request so the phone re-sends ONLY what we're missing (a
            // notify dropped in-air that the user never saw) — never re-popping a
            // notification already shown.
            // Newest-last, and BOUNDED. It was an unbounded HashSet, which is
            // what broke the catch-up request: the whole set was packed into a
            // single sealed frame — one ATT write, 512 bytes at this MTU — and
            // an Android SBN key runs about 45 bytes, so past roughly eight
            // displayed notifications the request no longer fit and BlueZ
            // refused it. The success timestamp is only stamped on a send that
            // went out, so every later nudge retried the same oversized frame
            // and failed identically: the recovery path was dead within an hour
            // of each session, permanently and silently.
            //
            // A deque also gives the order a HashSet never had, so "the keys
            // worth reconciling" means the most recent ones rather than
            // whichever the hasher happened to yield.
            let delivered_keys: Arc<tokio::sync::Mutex<std::collections::VecDeque<String>>> =
                Arc::new(tokio::sync::Mutex::new(std::collections::VecDeque::new()));
            // Debounce: coalesce a burst of drop nudges into ONE catch-up request.
            let catch_up_pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
            // Hard rate-limit on catch-up requests. A catch-up makes the phone
            // re-send its missing notifications (+ their icons) in a burst; on a
            // flaky link that very burst can drop a frame → resync → another
            // catch-up → a self-sustaining storm that flaps the BLE session (the
            // same failure `VortexStack` warns about for the companion refresh).
            // So fire at most once per this window, whatever the trigger — the
            // quiet gaps between let the link stay up and deliver live.
            let catch_up_last = Arc::new(tokio::sync::Mutex::new(
                None::<std::time::Instant>,
            ));
            const CATCH_UP_MIN_INTERVAL: std::time::Duration =
                std::time::Duration::from_secs(90);
            /// Most keys we track, and therefore the most a catch-up request can
            /// carry. Sized to stay inside a single 512-byte ATT write with the
            /// JSON envelope: ~170 bytes of boilerplate leaves room for six
            /// ~45-byte keys with margin.
            const CATCH_UP_MAX_KEYS: usize = 6;

            {
                let links = notif_links.clone();
                let app_show = app.clone();
                let writer_for_catchup = ble_notif_writer.clone();
                let delivered = delivered_keys.clone();
                let pending = catch_up_pending.clone();
                let catch_up_last = catch_up_last.clone();
                tokio::spawn(async move {
                    // Take down anything a previous run left on screen before
                    // we post anything new — see `sweep_stale`.
                    vortex_l3_daemon::core::notification_display::sweep_stale().await;
                    while let Some(notif) = ble_notif_rx.recv().await {
                        // Laptop-internal nudge (a BLE frame dropped, or we
                        // (re)connected): ask the phone to re-send any active
                        // notification we don't have. BLE Notify is fire-and-forget,
                        // so a dropped notification is otherwise lost forever. We
                        // carry our delivered keys so only the missing ones return.
                        if notif.resync {
                            if !NOTIF_SHOW.load(std::sync::atomic::Ordering::Relaxed) {
                                continue; // not showing phone notifs → nothing to reconcile
                            }
                            if !pending.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                let writer_h = writer_for_catchup.clone();
                                let delivered = delivered.clone();
                                let pending = pending.clone();
                                let catch_up_last = catch_up_last.clone();
                                tokio::spawn(async move {
                                    // Let the burst settle (and a fresh reconnect
                                    // publish its writer) before asking.
                                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                                    pending.store(false, std::sync::atomic::Ordering::SeqCst);
                                    // Rate-limit: skip if we asked recently, so a
                                    // flaky link's stream of drop/reconnect nudges
                                    // can't turn into a resend storm. (Only READ
                                    // here; the timestamp is stamped after a send
                                    // actually goes out, so a down link doesn't
                                    // block the next attempt.)
                                    if catch_up_last
                                        .lock()
                                        .await
                                        .map(|t| t.elapsed() < CATCH_UP_MIN_INTERVAL)
                                        == Some(true)
                                    {
                                        return;
                                    }
                                    // Link down → next drop/reconnect retries.
                                    let Some(w) = writer_h.lock().await.clone() else { return };
                                    // Cap the key list. It rides in ONE sealed
                                    // frame, which is one ATT write — 512 bytes
                                    // at this MTU. `delivered_keys` was never
                                    // pruned and was sent whole, and an Android
                                    // SBN key runs ~45 bytes, so past roughly
                                    // eight displayed notifications the request
                                    // simply exceeded the write and BlueZ
                                    // refused it. The timestamp is only stamped
                                    // on success, so every later nudge retried
                                    // the same oversized frame and failed the
                                    // same way: the whole recovery mechanism was
                                    // dead within an hour of each session.
                                    //
                                    // The newest keys are the ones worth
                                    // reconciling anyway — an older notification
                                    // the user never saw is not worth popping up
                                    // now.
                                    let known: Vec<String> =
                                        delivered.lock().await.iter().cloned().collect();
                                    let req = vortex_l3_daemon::core::notif_mirror::NotificationMirror {
                                        resync: true,
                                        known_keys: known,
                                        ..Default::default()
                                    };
                                    match w(req).await {
                                        Ok(()) => {
                                            *catch_up_last.lock().await = Some(std::time::Instant::now());
                                            tracing::info!(
                                                "notif catch-up requested (reconcile after BLE drop/reconnect)"
                                            );
                                        }
                                        Err(e) => tracing::warn!("notif catch-up request failed: {e}"),
                                    }
                                });
                            }
                            continue;
                        }
                        if notif.dismiss {
                            // Phone dismissed the original → close our copy.
                            // Resolve the id UNDER the lock, then drop the lock
                            // BEFORE the close() dbus round-trip (never hold the
                            // mutex across .await — it would stall every show()).
                            let id = {
                                let mut m = links.lock().await;
                                let found = m
                                    .iter()
                                    .find(|(_, v)| v.0 == notif.key)
                                    .map(|(&id, _)| id);
                                if let Some(id) = found {
                                    m.remove(&id);
                                }
                                found
                            };
                            if let Some(id) = id {
                                let _ = vortex_l3_daemon::core::notification_display::close(id).await;
                                vortex_l3_daemon::core::notification_display::forget_live_id(id);
                            }
                            continue;
                        }
                        // Per-device display toggle: drop silently when the user
                        // turned off showing phone notifications on THIS laptop.
                        if !NOTIF_SHOW.load(std::sync::atomic::Ordering::Relaxed) {
                            tracing::info!(app = %notif.app, "notif: suppressed (show toggle off)");
                            continue;
                        }
                        // The user is already READING this sender's chat (it's
                        // open and the window is focused) — a desktop popup for
                        // it is pure noise. Title match is how SMS notifications
                        // carry the sender.
                        {
                            use tauri::Manager;
                            let active = crate::ACTIVE_CHAT
                                .lock()
                                .map(|g| g.clone())
                                .unwrap_or_default();
                            if !active.is_empty()
                                && notif.title == active
                                && app_show
                                    .get_webview_window("main")
                                    .map(|w| w.is_focused().unwrap_or(false))
                                    .unwrap_or(false)
                            {
                                tracing::info!("notif: suppressed (chat open and focused)");
                                continue;
                            }
                        }
                        // Grouping (Telegram-style): if we already have a
                        // desktop notification for this chat (same phone key),
                        // update it in place instead of stacking a new one.
                        let replaces_id = if notif.key.is_empty() {
                            0
                        } else {
                            links
                                .lock()
                                .await
                                .iter()
                                .find(|(_, v)| v.0 == notif.key)
                                .map(|(&id, _)| id)
                                .unwrap_or(0)
                        };
                        match vortex_l3_daemon::core::notification_display::show(&notif, replaces_id)
                            .await
                        {
                            Ok(id) => {
                                tracing::info!(app = %notif.app, id, replaces_id, "notif: shown on desktop");
                                // Remember it on disk: gdbus posts these
                                // detached so they outlive us, and only this
                                // record lets a later run take down survivors
                                // whose action mappings died with the process.
                                vortex_l3_daemon::core::notification_display::remember_live_id(id);
                                if replaces_id != 0 && replaces_id != id {
                                    vortex_l3_daemon::core::notification_display::forget_live_id(
                                        replaces_id,
                                    );
                                }
                                if !notif.key.is_empty() {
                                    // Re-key under the (possibly new) id; drop the
                                    // old mapping if the id changed on replace.
                                    let mut m = links.lock().await;
                                    if replaces_id != 0 && replaces_id != id {
                                        m.remove(&replaces_id);
                                    }
                                    m.insert(
                                        id,
                                        (
                                            notif.key.clone(),
                                            notif.reply_index,
                                            std::time::Instant::now(),
                                            notif.title.clone(),
                                            notif.app_id.clone(),
                                            notif.app.clone(),
                                        ),
                                    );
                                }
                                // Record it as delivered (separate lock, after the
                                // links lock is dropped — never hold a mutex across
                                // this .await) so a catch-up won't re-request it.
                                if !notif.key.is_empty() {
                                    {
                                        let mut g = delivered.lock().await;
                                        if !g.contains(&notif.key) {
                                            g.push_back(notif.key.clone());
                                            while g.len() > CATCH_UP_MAX_KEYS {
                                                g.pop_front();
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => tracing::warn!("desktop notification failed: {e}"),
                        }
                    }
                });
            }

            // Laptop→phone notification mirroring: a dbus-monitor capture task
            // forwards THIS laptop's desktop notifications here; a consumer
            // writes each to the phone over the live BLE link. `ble_notif_writer`
            // (declared above, shared with the catch-up requester) is
            // published/cleared by the persistent loop on connect/disconnect.
            {
                let (cap_tx, mut cap_rx) = tokio::sync::mpsc::unbounded_channel::<
                    vortex_l3_daemon::core::notif_mirror::NotificationMirror,
                >();
                vortex_l3_daemon::core::notif_capturer::spawn(cap_tx);
                let writer_handle = ble_notif_writer.clone();
                tokio::spawn(async move {
                    while let Some(notif) = cap_rx.recv().await {
                        // Per-device send toggle: drop when the laptop user turned
                        // off forwarding its notifications.
                        if !NOTIF_SEND.load(std::sync::atomic::Ordering::Relaxed) {
                            continue;
                        }
                        let writer = writer_handle.lock().await.clone();
                        if let Some(w) = writer {
                            if let Err(e) = w(notif).await {
                                tracing::warn!("laptop→phone notif write failed: {e}");
                            }
                        }
                        // No live BLE link → silently drop (notifications are
                        // ephemeral; no point queueing a backlog).
                    }
                });
            }

            // Dismissal sync (laptop→phone leg): watch NotificationClosed; when
            // the user dismisses a MIRRORED phone notification on this laptop
            // (reason==2), tell the phone to clear the original. reason==3 is our
            // own close() (phone already dismissed it) — just forget the link.
            {
                let (closed_tx, mut closed_rx) =
                    tokio::sync::mpsc::unbounded_channel::<(u32, u32)>();
                tokio::spawn(vortex_l3_daemon::core::notification_display::watch_closed(closed_tx));
                let links = notif_links.clone();
                let recent_actions = notif_recent_actions.clone();
                let writer_handle = ble_notif_writer.clone();
                tokio::spawn(async move {
                    while let Some((id, reason)) = closed_rx.recv().await {
                        // reason 2 = dismissed-by-user. Two complications on GNOME:
                        //  (a) clicking an action ALSO closes with reason 2 (the
                        //      ActionInvoked races this close); and
                        //  (b) GNOME (its notification relay) emits a PHANTOM
                        //      reason-2 close within a few ms of EVERY notification —
                        //      not a real dismissal at all.
                        // So defer briefly, then only sync a dismissal to the phone
                        // when it's neither an action side-effect NOR a phantom
                        // (i.e. the close lands well after we showed the notif).
                        if reason != 2 {
                            // 1=expired, 3=our own close() → just forget the link.
                            links.lock().await.remove(&id);
                            continue;
                        }
                        let links = links.clone();
                        let recent_actions = recent_actions.clone();
                        let writer_handle = writer_handle.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                            let was_action = recent_actions.lock().await.remove(&id);
                            // Peek (don't remove yet) so the phantom-close filter can
                            // keep the link alive for a genuine later dismissal.
                            let link = links.lock().await.get(&id).cloned();
                            let Some((key, _reply_index, shown_at, _title, _app_id, _app_label)) = link else {
                                return;
                            };
                            if was_action {
                                // Action's side-effect close → drop the link, no dismiss.
                                links.lock().await.remove(&id);
                                return;
                            }
                            // Phantom GNOME close fires ~ms after show → ignore it and
                            // KEEP the notification so it isn't cancelled on the phone.
                            if shown_at.elapsed() < std::time::Duration::from_millis(1200) {
                                return;
                            }
                            // Real user dismissal on this laptop → sync to the phone.
                            links.lock().await.remove(&id);
                            if key.is_empty() {
                                return;
                            }
                            let writer = writer_handle.lock().await.clone();
                            if let Some(w) = writer {
                                let dismiss = vortex_l3_daemon::core::notif_mirror::NotificationMirror {
                                    key,
                                    dismiss: true,
                                    ..Default::default()
                                };
                                let _ = w(dismiss).await;
                            }
                        });
                    }
                });
            }

            // Action buttons (phone→laptop): when the user clicks an action on a
            // mirrored notification, ask the phone to fire it. The "act:<n>" key
            // maps straight to the phone's action index. Reply text isn't carried
            // (no portable inline-reply on freedesktop) — a plain fire.
            {
                let (act_tx, mut act_rx) = tokio::sync::mpsc::unbounded_channel::<(u32, String)>();
                tokio::spawn(vortex_l3_daemon::core::notification_display::watch_actions(act_tx));
                let links = notif_links.clone();
                let recent_actions = notif_recent_actions.clone();
                let writer_handle = ble_notif_writer.clone();
                let app_open = app.clone();
                tokio::spawn(async move {
                    while let Some((id, action_key)) = act_rx.recv().await {
                        // Clicking the notification BODY: focus the app and
                        // hand the UI the title so it can jump to the chat
                        // (the frontend resolves title → number via its
                        // contacts/conversations and falls back to just
                        // focusing when nothing matches).
                        if action_key == "default" {
                            let (title, app_id, app_label) = links
                                .lock()
                                .await
                                .get(&id)
                                .map(|l| (l.3.clone(), l.4.clone(), l.5.clone()))
                                .unwrap_or_default();
                            // Resolve what THIS app's click opens on the laptop,
                            // generic→specific (SMS/call page → WhatsApp deep link
                            // → webmail inbox → launch a matching desktop app →
                            // nothing). Only the SMS/call pages pop the Vortex
                            // window; opening an external app/URL must not hijack
                            // focus onto Vortex.
                            match resolve_notif_click(&app_id, &app_label, &title) {
                                ClickAction::Page(kind) => {
                                    use tauri::Emitter;
                                    crate::window::present_main(&app_open);
                                    tracing::info!(%app_id, kind, "notif click: open laptop page");
                                    let _ = app_open.emit(
                                        "vortex:open-chat",
                                        serde_json::json!({ "title": title, "appId": app_id, "kind": kind }),
                                    );
                                }
                                ClickAction::OpenUrl(url) => {
                                    tracing::info!(%app_id, "notif click: open in browser");
                                    let _ = tokio::process::Command::new("xdg-open").arg(&url).spawn();
                                }
                                ClickAction::LaunchApp(path) => {
                                    tracing::info!(%app_id, app = %app_label, "notif click: launch desktop app");
                                    crate::desktop_apps::launch(&path);
                                }
                                ClickAction::Dismiss => {
                                    tracing::info!(%app_id, "notif click: dismiss-only (no matching laptop app)");
                                }
                            }
                            continue;
                        }
                        let Some(idx) = action_key.strip_prefix("act:").and_then(|s| s.parse::<i32>().ok())
                        else {
                            continue;
                        };
                        // Mark this id so the paired NotificationClosed(reason=2)
                        // isn't mirrored to the phone as a dismiss. Self-evicting:
                        // the close consumer removes it; if no close follows, drop
                        // it after a grace window so the set can't grow unbounded.
                        {
                            let recent = recent_actions.clone();
                            recent.lock().await.insert(id);
                            tokio::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                recent.lock().await.remove(&id);
                            });
                        }
                        // Read (don't remove) the link — the racing close mustn't
                        // strip it before we resolve the phone key + reply index.
                        let link = links.lock().await.get(&id).cloned();
                        let Some((key, reply_index, _shown_at, _title, _app_id, _app_label)) = link else {
                            continue;
                        };
                        // Keep the writer as an Option (don't bail when it's None):
                        // a down BLE link must still fall back to the LAN backstop.
                        let writer = writer_handle.lock().await.clone();
                        // A reply (RemoteInput) action gets a typed-text entry; a
                        // plain action fires immediately. Run off the loop so a
                        // dialog left open doesn't stall other action clicks. A
                        // cancelled/empty reply fires nothing (the user backed out).
                        let needs_reply = idx == reply_index;
                        tokio::spawn(async move {
                            let reply = if needs_reply {
                                match prompt_reply_text("Type your reply:").await {
                                    Some(t) => t,
                                    None => return,
                                }
                            } else {
                                String::new()
                            };
                            let seq = next_invoke_seq();
                            let invoke = vortex_l3_daemon::core::notif_mirror::NotificationMirror {
                                key,
                                invoke_index: idx,
                                reply,
                                seq,
                                ..Default::default()
                            };
                            // BLE-FIRST: send over the BLE NOTIFICATION frame. A
                            // dropped frame is recovered by the phone's nonce-resync,
                            // so an Ok write means delivered — LAN stays idle.
                            let ble_ok = match writer {
                                Some(w) => w(invoke.clone()).await.is_ok(),
                                None => false, // BLE link down
                            };
                            // LAN backstop ONLY when BLE is absent/failed (wedge or
                            // link down). The phone dedups by seq, so even if both
                            // somehow land it acts once.
                            if !ble_ok {
                                if let Ok(mut g) = PENDING_NOTIF_INVOKE.lock() {
                                    *g = Some(invoke);
                                }
                                if let Some(n) = crate::SYNC_NUDGE.get() {
                                    n.notify_one();
                                }
                            }
                        });
                    }
                });
            }
    (ble_notif_tx, ble_notif_writer)
}
