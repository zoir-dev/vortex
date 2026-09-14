//! Desktop notifications, through the platform seam.
//!
//! Every consumer in the app used to call `core::notification_display` — the
//! freedesktop D-Bus implementation — directly. These wrappers go through
//! `core::platform::notifier()` instead, so the same call reaches libnotify on
//! Linux and a WinRT toast on Windows, and the `fc:` / `call:` / `act:` action
//! keys come back through one stream either way.
//!
//! # Why `show_mirror` still forks
//!
//! A mirrored phone notification carries the phone app's real logo (sent over
//! BLE as ICON chunks and cached on disk) and a per-app header name. Both are
//! freedesktop hints with no toast equivalent — Windows takes a notification's
//! icon and name from the AUMID's shortcut, not from the notification. So the
//! Linux path keeps using `notification_display::show`, which knows about the
//! icon cache, and elsewhere the mirror is flattened onto the plain seam call.
//! The visible difference is a Windows toast showing the Vortex icon instead of
//! Telegram's.

use vortex_l3_daemon::core::notif_mirror::NotificationMirror;
use vortex_l3_daemon::core::platform::notifier;

/// Show (or replace) a notification with optional action buttons.
///
/// `urgent` keeps it on screen until acted on — call banners and consent
/// prompts, where a notification that vanishes is worse than none.
pub(crate) async fn show_banner(
    summary: &str,
    body: &str,
    app_id: &str,
    actions: &[(String, String)],
    replaces_id: u32,
    urgent: bool,
) -> Result<u32, String> {
    notifier()
        .show(summary, body, app_id, actions, replaces_id, urgent)
        .await
}

/// Withdraw a notification we showed.
pub(crate) async fn close(id: u32) -> Result<(), String> {
    notifier().close(id).await
}

/// Subscribe to button clicks: `(notification id, action key)`.
pub(crate) fn watch_actions(tx: tokio::sync::mpsc::UnboundedSender<(u32, String)>) {
    notifier().actions(tx);
}

/// Subscribe to dismissals: `(notification id, reason)`.
pub(crate) fn watch_closed(tx: tokio::sync::mpsc::UnboundedSender<(u32, u32)>) {
    notifier().closures(tx);
}

/// Show a mirrored phone notification.
#[cfg(target_os = "linux")]
pub(crate) async fn show_mirror(notif: &NotificationMirror, replaces_id: u32) -> Result<u32, String> {
    vortex_l3_daemon::core::notification_display::show(notif, replaces_id).await
}

/// Show a mirrored phone notification, flattened onto the seam.
///
/// Keeps the two things the consumers depend on: the summary/body split (title,
/// falling back to the app name when the phone sent none), and the `act:<index>`
/// action keys — the index is what gets sent back to the phone, so it must
/// survive. Dropped: the per-app icon and header name, which have no toast
/// equivalent.
#[cfg(not(target_os = "linux"))]
pub(crate) async fn show_mirror(notif: &NotificationMirror, replaces_id: u32) -> Result<u32, String> {
    let summary = if notif.title.is_empty() {
        notif.app.clone()
    } else {
        notif.title.clone()
    };
    // Same keying as the Linux path: "act:<i>" maps straight back to the phone's
    // action index, and consumers filter on that prefix.
    let actions: Vec<(String, String)> = notif
        .actions
        .iter()
        .enumerate()
        .map(|(i, label)| (format!("act:{i}"), label.clone()))
        .collect();
    // Actionable ones stay up until answered, like the Linux `expire_timeout`
    // of 0 for the same case.
    let urgent = !notif.actions.is_empty();
    notifier()
        .show(&summary, &notif.text, "vortex", &actions, replaces_id, urgent)
        .await
}
