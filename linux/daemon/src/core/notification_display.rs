//! Show a mirrored phone notification on the laptop desktop via the
//! standard `org.freedesktop.Notifications` D-Bus service (the same bus
//! `notify-send` uses). Best-effort — a missing notification daemon just
//! logs a warning; it never gates anything.
//!
//! Notifications that carry action buttons (incoming-call banners, incoming-file
//! consent) are the delicate case: GNOME Shell and Plasma disagree about what
//! the SENDER must look like for the buttons to show up, so [`notify`] probes
//! the running server once and posts accordingly — see [`post_via_gdbus_child`].

use std::collections::HashMap;

use zbus::zvariant::Value;
use zbus::{Connection, Proxy};

use crate::core::notif_mirror::NotificationMirror;

/// Quote + escape a string as a GVariant string literal for a `gdbus call`
/// argument, so arbitrary notification text (quotes, brackets, backslashes)
/// is passed verbatim and never misparsed.
fn gvariant_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Process-lifetime session-bus connection shared by every call in here. It
/// MUST outlive the notifications it posts: servers key a notification's action
/// buttons to the sending bus name, so a connection opened per call and dropped
/// on return is a sender that has already vanished by the time the user looks at
/// the banner — see [`post_via_gdbus_child`].
static CONN: tokio::sync::OnceCell<Connection> = tokio::sync::OnceCell::const_new();

async fn conn() -> Result<&'static Connection, String> {
    CONN.get_or_try_init(|| async {
        Connection::session()
            .await
            .map_err(|e| format!("session bus: {e}"))
    })
    .await
}

async fn notifications_proxy() -> Result<Proxy<'static>, String> {
    Proxy::new(
        conn().await?,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await
    .map_err(|e| format!("notifications proxy: {e}"))
}

/// How to post: the two big shells want OPPOSITE things from the sender.
///
/// * **GNOME Shell** associates a notification with its SENDER process. Because
///   our sender owns a (Tauri) window, gnome-shell instantly auto-dismisses our
///   notifications (NotificationClosed reason=2 within ~ms, as if the user had
///   already seen them) so they never appear at all. A *windowless* sender — a
///   short-lived `gdbus` child — has no window to associate, so the banner stays.
/// * **Plasma** (and any server that watches the sender) does the reverse: when
///   the sending process leaves the bus it strips the notification's action
///   buttons, since a click could no longer be delivered to anyone. Posting from
///   a transient `gdbus` child there yields a banner with NOTHING to click — the
///   incoming-file consent prompt can then never be accepted.
///
/// So: `gdbus` child on GNOME Shell only, our own long-lived connection
/// everywhere else. Either way ActionInvoked / NotificationClosed are broadcast
/// signals caught by the global sender-less watchers below, so routing a click
/// never depends on who posted the notification.
async fn post_via_gdbus_child() -> bool {
    static VIA_CHILD: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
    *VIA_CHILD
        .get_or_init(|| async {
            let info = match notifications_proxy().await {
                Ok(p) => p
                    .call::<_, _, (String, String, String, String)>("GetServerInformation", &())
                    .await
                    .map_err(|e| format!("GetServerInformation: {e}")),
                Err(e) => Err(e),
            };
            match info {
                Ok((name, vendor, version, _spec)) => {
                    let gnome = format!("{name} {vendor}").to_lowercase().contains("gnome");
                    tracing::info!(
                        server = %name, vendor = %vendor, %version, via_gdbus_child = gnome,
                        "notification server probed"
                    );
                    if let Ok(p) = notifications_proxy().await {
                        if let Ok(caps) = p.call::<_, _, Vec<String>>("GetCapabilities", &()).await {
                            if !caps.iter().any(|c| c == "actions") {
                                tracing::warn!(
                                    ?caps,
                                    "notification server does NOT support actions — banners with \
                                     Accept/Decline (incoming file shares, calls) will have no \
                                     buttons; those prompts will time out as declined"
                                );
                            }
                        }
                    }
                    gnome
                }
                // Can't tell → keep the historical GNOME-safe path.
                Err(e) => {
                    tracing::warn!("notification server probe failed ({e}); assuming GNOME");
                    true
                }
            }
        })
        .await
}

/// Post (or update in place, when `replaces_id` != 0) one notification and
/// return its id. `actions` is the freedesktop flat `[key, label, key, label, …]`
/// array. Transport picked by [`post_via_gdbus_child`].
#[allow(clippy::too_many_arguments)]
async fn notify(
    app_name: &str,
    replaces_id: u32,
    app_icon: &str,
    summary: &str,
    body: &str,
    actions: &[String],
    urgency: Option<u8>,
    category: Option<&str>,
    expire_timeout: i32,
) -> Result<u32, String> {
    if !post_via_gdbus_child().await {
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        if let Some(u) = urgency {
            hints.insert("urgency", Value::U8(u));
        }
        if let Some(c) = category {
            hints.insert("category", Value::from(c));
        }
        return notifications_proxy()
            .await?
            .call::<_, _, u32>(
                "Notify",
                &(
                    app_name,
                    replaces_id,
                    app_icon,
                    summary,
                    body,
                    actions,
                    hints,
                    expire_timeout,
                ),
            )
            .await
            .map_err(|e| format!("Notify: {e}"));
    }

    let actions_arg = format!(
        "[{}]",
        actions
            .iter()
            .map(|a| gvariant_string(a))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut hint_parts: Vec<String> = Vec::new();
    if let Some(u) = urgency {
        hint_parts.push(format!("'urgency': <byte {u}>"));
    }
    if let Some(c) = category {
        hint_parts.push(format!("'category': <{}>", gvariant_string(c)));
    }
    // A bare `{}` is an ambiguous GVariant, so empty hints need the type prefix.
    let hints_arg = if hint_parts.is_empty() {
        "@a{sv} {}".to_string()
    } else {
        format!("{{{}}}", hint_parts.join(", "))
    };
    let output = tokio::process::Command::new("gdbus")
        .arg("call")
        .arg("--session")
        .arg("--dest")
        .arg("org.freedesktop.Notifications")
        .arg("--object-path")
        .arg("/org/freedesktop/Notifications")
        .arg("--method")
        .arg("org.freedesktop.Notifications.Notify")
        .arg(gvariant_string(app_name))
        .arg(replaces_id.to_string()) // u32: 0 = new, else update in place
        .arg(gvariant_string(app_icon))
        .arg(gvariant_string(summary))
        .arg(gvariant_string(body))
        .arg(&actions_arg) // as
        .arg(&hints_arg) // a{sv}
        .arg(expire_timeout.to_string()) // i32
        .output()
        .await
        .map_err(|e| format!("gdbus spawn: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "gdbus Notify failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    // gdbus prints "(uint32 78,)" — strip the type keyword first (it ends in
    // digits "32" which would otherwise be misread as the id), then take the
    // remaining integer.
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .replace("uint32", " ")
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .map_err(|_| format!("gdbus Notify: unparseable id {stdout:?}"))
}

/// Pop a desktop notification for a mirrored phone notification. The phone
/// app label becomes the summary prefix so the user sees which app it's
/// from. Content is taken as-is (already length-capped on the phone).
/// Returns the assigned notification id (for dismissal sync — we map it to
/// the phone's notification key).
pub async fn show(notif: &NotificationMirror, replaces_id: u32) -> Result<u32, String> {
    // `replaces_id` (0 = new) collapses repeated notifications from the same
    // chat into one that updates in place — standard messenger-style grouping.
    // The app name goes in the header (app_name), so the summary is just the
    // title (chat / sender), falling back to the app label when there's no
    // title. body = the notification text.
    let summary = if notif.title.is_empty() {
        notif.app.clone()
    } else {
        notif.title.clone()
    };

    // Action buttons: the freedesktop "actions" array is [key, label, key,
    // label, …]. We key each as "act:<index>" so ActionInvoked maps straight
    // back to the phone action index. The well-known "default" key makes the
    // notification BODY clickable (the server fires ActionInvoked("default")
    // on click) — the UI uses it to focus the app and jump to the chat.
    let mut actions: Vec<String> = vec!["default".to_string(), "Open".to_string()];
    for (i, label) in notif.actions.iter().enumerate() {
        actions.push(format!("act:{i}"));
        actions.push(label.clone());
    }
    // Actionable notifications stay until dismissed (0); plain ones get a
    // short banner (GNOME drops a banner's buttons when it expires).
    let expire_timeout: i32 = if notif.actions.is_empty() { 8000 } else { 0 };

    // app_icon: the phone's real app logo if we've cached it (sent once over
    // BLE as ICON chunks), else the bundled generic bell. ALWAYS an absolute
    // path under ~/.cache/vortex/ so the laptop→phone capturer can recognise
    // (and skip) our own notifications by the icon path.
    let app_icon = crate::core::icon_cache::icon_path(&notif.app_id)
        .filter(|p| p.exists())
        .or_else(crate::core::icon_cache::ensure_generic)
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "phone-symbolic".to_string());
    // Header app name = the real phone app (e.g. "Telegram"), not "Vortex".
    let app_name = if notif.app.is_empty() {
        "Phone".to_string()
    } else {
        notif.app.clone()
    };

    // GNOME collapses newlines in a notification body to spaces (even
    // notify-send can't line-break), so stacked chat messages would run
    // together. Use a middle-dot separator so they stay distinguishable
    // on the one line GNOME gives us. (A future GNOME Shell extension
    // could render the raw newlines as real lines.)
    let body = notif.text.replace('\n', "  ·  ");
    notify(
        &app_name, // = real phone app, not "Vortex"
        replaces_id,
        &app_icon,
        &summary,
        &body,
        &actions,
        None,
        None,
        expire_timeout,
    )
    .await
}

/// Close a desktop notification we previously showed (the phone dismissed
/// its original, so drop our mirrored copy). Emits a NotificationClosed
/// signal with reason=3 (closed-by-call), which the watcher ignores.
pub async fn close(id: u32) -> Result<(), String> {
    notifications_proxy()
        .await?
        .call::<_, _, ()>("CloseNotification", &(id,))
        .await
        .map_err(|e| format!("CloseNotification: {e}"))?;
    Ok(())
}

/// Pop (or update) the continuity-style incoming-call banner: a
/// persistent, critical-urgency desktop notification with caller info and
/// action buttons (Accept / Decline / …). `actions` is `(key, label)` pairs —
/// the call module keys them `call:<verb>` so its own ActionInvoked watcher
/// can route the click straight to a `CallControl` (disjoint from the
/// notification-mirror's `act:<n>` keys, so the two watchers never collide).
/// `replaces_id` (0 = new) updates the same banner in place across phases.
/// Posted through [`notify`], which picks the transport the local shell needs
/// for the action buttons to actually appear and stay clickable.
pub async fn show_call_banner(
    title: &str,
    body: &str,
    app_id: &str,
    actions: &[(String, String)],
    replaces_id: u32,
    critical: bool,
) -> Result<u32, String> {
    // The phone's real dialer-app logo if we've cached it, else a themed call
    // glyph. Same cache the notification mirror fills over BLE.
    let icon = crate::core::icon_cache::icon_path(app_id)
        .filter(|p| p.exists())
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "call-start-symbolic".to_string());
    // Flatten (key, label) pairs into the freedesktop [key, label, …] array.
    let mut flat: Vec<String> = Vec::with_capacity(actions.len() * 2);
    for (key, label) in actions {
        flat.push(key.clone());
        flat.push(label.clone());
    }

    // urgency=critical (byte 2) → GNOME keeps the banner on screen until acted
    // on. When the user dismisses it (the "silence" gesture) we re-show at
    // urgency=normal (byte 1): it tucks quietly into the notification list (no
    // aggressive re-pop) but stays there with its Accept/Decline actions.
    // category 'call.incoming' lets shells style it.
    let urgency = if critical { 2u8 } else { 1u8 };

    notify(
        "Phone", // app_name
        replaces_id,
        &icon, // real dialer logo or call glyph
        title, // summary = caller
        body,  // "Incoming call" / number
        &flat,
        Some(urgency),
        Some("call.incoming"),
        0, // expire_timeout: never
    )
    .await
}

/// Build a sender-less signal match rule for one of the Notifications
/// signals. CRUCIAL on GNOME: the `org.freedesktop.Notifications` well-known
/// name is owned by a relay (e.g. `:1.34`) that forwards to gnome-shell,
/// but the ActionInvoked / NotificationClosed signals are emitted by
/// gnome-shell under a DIFFERENT unique name (e.g. `:1.26`). A Proxy-based
/// subscription pins `sender=org.freedesktop.Notifications` → resolves to
/// the relay's name → filters OUT gnome-shell's signals, so we never see
/// them. Matching on interface+member+path only (no sender) catches the
/// real emitter.
fn signal_rule(member: &'static str) -> zbus::Result<zbus::MatchRule<'static>> {
    Ok(zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.Notifications")?
        .member(member)?
        .path("/org/freedesktop/Notifications")?
        .build())
}

/// Watch the `NotificationClosed(id, reason)` signal and forward each event
/// to `tx`. reason: 1=expired, 2=dismissed-by-user, 3=closed-by-CloseNotification,
/// 4=undefined. The caller uses reason==2 to sync a user dismissal back to
/// the phone (and ignores 3, which is our own [`close`]).
pub async fn watch_closed(tx: tokio::sync::mpsc::UnboundedSender<(u32, u32)>) {
    use futures::StreamExt;
    let conn = match Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("notif-closed watch: session bus: {e}");
            return;
        }
    };
    let rule = match signal_rule("NotificationClosed") {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("notif-closed watch: rule: {e}");
            return;
        }
    };
    let mut stream = match zbus::MessageStream::for_match_rule(rule, &conn, None).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("notif-closed watch: subscribe: {e}");
            return;
        }
    };
    tracing::info!("notif-closed watch: subscribed (sender-less rule)");
    while let Some(Ok(msg)) = stream.next().await {
        if let Ok((id, reason)) = msg.body().deserialize::<(u32, u32)>() {
            tracing::info!(id, reason, "notif-closed: NotificationClosed signal");
            let _ = tx.send((id, reason));
        }
    }
}

/// Watch the `ActionInvoked(id, action_key)` signal — the user clicked an
/// action button on a mirrored notification. Forwards (id, action_key) to
/// `tx`; the caller maps the id back to the phone key and the "act:<n>"
/// key to the action index, then asks the phone to fire it.
pub async fn watch_actions(tx: tokio::sync::mpsc::UnboundedSender<(u32, String)>) {
    use futures::StreamExt;
    let conn = match Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("notif-action watch: session bus: {e}");
            return;
        }
    };
    let rule = match signal_rule("ActionInvoked") {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("notif-action watch: rule: {e}");
            return;
        }
    };
    let mut stream = match zbus::MessageStream::for_match_rule(rule, &conn, None).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("notif-action watch: subscribe: {e}");
            return;
        }
    };
    tracing::info!("notif-action watch: subscribed (sender-less rule)");
    while let Some(Ok(msg)) = stream.next().await {
        if let Ok((id, key)) = msg.body().deserialize::<(u32, String)>() {
            tracing::info!(id, action = %key, "notif-action: ActionInvoked signal");
            let _ = tx.send((id, key));
        }
    }
}
