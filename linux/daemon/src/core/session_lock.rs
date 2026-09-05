//! Lock / unlock the local desktop session — the companion remote-lock
//! feature (phone button → laptop lock screen).
//!
//! Lock rides the session bus (`org.gnome.ScreenSaver.Lock`) — unprivileged
//! and honored by GNOME Shell — with a logind `Session.Lock` fallback for
//! other desktops. Unlock has NO unprivileged session-bus API by design
//! (gnome-shell ignores `ScreenSaver.SetActive(false)`); the supported path
//! is logind's `Session.Unlock` on the system bus, which GNOME Shell honors
//! by dismissing the shield without a password. That call is gated by the
//! polkit action `org.freedesktop.login1.lock-sessions` (auth_admin_keep by
//! default), so remote unlock needs a one-time rule allowing this user.
//!
//! That rule is NOT hand-written and NOT installed by `install_linux.sh` —
//! it writes to /etc and changes what the system authorises, so it is opt-in:
//!
//! ```text
//! sudo linux/packaging/install-unlock-rule.sh            # install
//! sudo linux/packaging/install-unlock-rule.sh --remove   # undo
//!      linux/packaging/install-unlock-rule.sh --status   # is it on?
//! ```
//!
//! Until it is installed `unlock()` fails with an access-denied error and
//! [`is_unlock_denied`] identifies that case, so callers can say so out loud
//! instead of leaving a toggle that silently does nothing.
//!
//! Security model: the command only ever arrives over the Noise-
//! authenticated transport from an already-trusted peer, and the caller
//! (ui-tauri) dedups by a monotonic sequence number — see
//! `AppState.lock_command` / `lock_command_seq`.

use std::os::unix::fs::MetadataExt;

use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, Proxy};

const LOGIN1: &str = "org.freedesktop.login1";

/// Object path of this uid's graphical (seated) logind session. Resolved
/// via `ListSessions` instead of `GetSession("auto")` because the daemon
/// usually runs inside the user-manager scope (terminal / app launcher),
/// where "auto" can't map the caller to the seat session.
async fn graphical_session_path(conn: &Connection) -> Result<OwnedObjectPath, String> {
    let mgr = Proxy::new(
        conn,
        LOGIN1,
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    .map_err(|e| format!("login1 manager proxy: {e}"))?;
    // a(susso): (session_id, uid, user, seat, object_path)
    let sessions: Vec<(String, u32, String, String, OwnedObjectPath)> = mgr
        .call("ListSessions", &())
        .await
        .map_err(|e| format!("ListSessions: {e}"))?;
    let my_uid = std::fs::metadata("/proc/self")
        .map_err(|e| format!("uid lookup: {e}"))?
        .uid();
    sessions
        .iter()
        .find(|(_, uid, _, seat, _)| *uid == my_uid && !seat.is_empty())
        .or_else(|| sessions.iter().find(|(_, uid, ..)| *uid == my_uid))
        .map(|s| s.4.clone())
        .ok_or_else(|| "no logind session for this user".to_string())
}

async fn session_proxy(conn: &Connection) -> Result<Proxy<'static>, String> {
    let path = graphical_session_path(conn).await?;
    Proxy::new(conn, LOGIN1, path, "org.freedesktop.login1.Session")
        .await
        .map_err(|e| format!("login1 session proxy: {e}"))
}

/// Lock the desktop session. GNOME path first (unprivileged), logind
/// `Session.Lock` as the cross-desktop fallback.
pub async fn lock() -> Result<(), String> {
    if let Ok(conn) = Connection::session().await {
        if let Ok(p) = Proxy::new(
            &conn,
            "org.gnome.ScreenSaver",
            "/org/gnome/ScreenSaver",
            "org.gnome.ScreenSaver",
        )
        .await
        {
            if p.call::<_, _, ()>("Lock", &()).await.is_ok() {
                return Ok(());
            }
        }
    }
    let conn = Connection::system()
        .await
        .map_err(|e| format!("system bus: {e}"))?;
    let session = session_proxy(&conn).await?;
    session
        .call::<_, _, ()>("Lock", &())
        .await
        .map_err(|e| format!("logind Lock: {e}"))
}

/// True when an `unlock()` error is polkit REFUSING the call rather than a
/// transport or session-lookup problem — i.e. the one-time rule (see module
/// doc) was never installed. The distinction matters to callers: a refusal is
/// permanent and actionable by the user, everything else is worth a retry.
///
/// Matched on substrings because the exact wording differs between the D-Bus
/// error name, polkit's own message, and older systemd versions.
pub fn is_unlock_denied(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("accessdenied")
        || e.contains("access denied")
        || e.contains("interactiveauthorizationrequired")
        || e.contains("interactive authentication")
        || e.contains("notauthorized")
        || e.contains("not authorized")
        || e.contains("permission denied")
}

/// Unlock the desktop session via logind. Fails with an access-denied
/// error until the one-time polkit rule is installed (see module doc);
/// [`is_unlock_denied`] recognises exactly that failure.
pub async fn unlock() -> Result<(), String> {
    let conn = Connection::system()
        .await
        .map_err(|e| format!("system bus: {e}"))?;
    let session = session_proxy(&conn).await?;
    session
        .call::<_, _, ()>("Unlock", &())
        .await
        .map_err(|e| {
            format!(
                "logind Unlock: {e} (missing the one-time rule? \
                 run: sudo linux/packaging/install-unlock-rule.sh)"
            )
        })
}

/// Current `LockedHint` of the graphical session — what we report to the
/// phone as the laptop's locked/unlocked state. `None` when logind is
/// unreachable or no session was found.
pub async fn locked_hint() -> Option<bool> {
    let conn = Connection::system().await.ok()?;
    let session = session_proxy(&conn).await.ok()?;
    session.get_property::<bool>("LockedHint").await.ok()
}

const IDLE_MONITOR_DEST: &str = "org.gnome.Mutter.IdleMonitor";
const IDLE_MONITOR_PATH: &str = "/org/gnome/Mutter/IdleMonitor/Core";
const IDLE_MONITOR_IFACE: &str = "org.gnome.Mutter.IdleMonitor";

/// Milliseconds since the last user input, across desktops. Tries, in order:
/// GNOME's Mutter IdleMonitor (precise), the freedesktop `ScreenSaver`
/// `GetSessionIdleTime` (KDE / XScreenSaver), then logind's coarse
/// `IdleSinceHint` (systemd, any DE). `None` only when none are reachable —
/// callers treat that as "idle state unknown" and skip idle-gated behaviour.
pub async fn idle_ms() -> Option<u64> {
    if let Some(ms) = mutter_idle_ms().await {
        return Some(ms);
    }
    if let Some(ms) = freedesktop_idle_ms().await {
        return Some(ms);
    }
    logind_idle_ms().await
}

/// GNOME: Mutter IdleMonitor `GetIdletime`. `None` off GNOME.
async fn mutter_idle_ms() -> Option<u64> {
    let conn = Connection::session().await.ok()?;
    let p = Proxy::new(&conn, IDLE_MONITOR_DEST, IDLE_MONITOR_PATH, IDLE_MONITOR_IFACE)
        .await
        .ok()?;
    p.call::<_, _, u64>("GetIdletime", &()).await.ok()
}

/// KDE / XScreenSaver: the legacy `org.freedesktop.ScreenSaver` exposes
/// `GetSessionIdleTime` (seconds). GNOME's implementation only does idle
/// INHIBIT and errors on this call, so it naturally returns `None` there.
async fn freedesktop_idle_ms() -> Option<u64> {
    let conn = Connection::session().await.ok()?;
    for path in ["/org/freedesktop/ScreenSaver", "/ScreenSaver"] {
        if let Ok(p) = Proxy::new(
            &conn,
            "org.freedesktop.ScreenSaver",
            path,
            "org.freedesktop.ScreenSaver",
        )
        .await
        {
            if let Ok(secs) = p.call::<_, _, u32>("GetSessionIdleTime", &()).await {
                return Some(secs as u64 * 1000);
            }
        }
    }
    None
}

/// systemd-logind fallback: `IdleHint` + `IdleSinceHint` (µs since the epoch)
/// on the graphical session. Coarse — the DE sets the hint — but present on any
/// systemd desktop when no finer source exists.
async fn logind_idle_ms() -> Option<u64> {
    let conn = Connection::system().await.ok()?;
    let session = session_proxy(&conn).await.ok()?;
    if !session.get_property::<bool>("IdleHint").await.ok()? {
        return Some(0); // not idle = active
    }
    let since_us = session.get_property::<u64>("IdleSinceHint").await.ok()?;
    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_micros() as u64;
    Some(now_us.saturating_sub(since_us) / 1000)
}

/// Fire `on_active` each time the user becomes active (key / mouse / wake) —
/// the proximity-unlock "the user woke the laptop" trigger. GNOME uses Mutter's
/// event-driven watch; other desktops fall back to polling [`idle_ms`] for the
/// active edge. Probes Mutter up front so `on_active` commits to exactly one
/// backend.
pub async fn watch_user_active(on_active: impl Fn() + Send + 'static) -> Result<(), String> {
    if mutter_idle_ms().await.is_some() {
        watch_user_active_mutter(on_active).await
    } else {
        watch_user_active_poll(on_active).await
    }
}

/// GNOME path: Mutter's `AddUserActiveWatch` is ONE-SHOT, so we re-arm after
/// every fire. Runs until the bus connection dies; Err only when the first
/// subscription can't be set up.
async fn watch_user_active_mutter(on_active: impl Fn() + Send + 'static) -> Result<(), String> {
    use futures::StreamExt;
    let conn = Connection::session()
        .await
        .map_err(|e| format!("session bus: {e}"))?;
    let proxy = Proxy::new(&conn, IDLE_MONITOR_DEST, IDLE_MONITOR_PATH, IDLE_MONITOR_IFACE)
        .await
        .map_err(|e| format!("idle-monitor proxy: {e}"))?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(IDLE_MONITOR_IFACE)
        .and_then(|b| b.member("WatchFired"))
        .and_then(|b| b.path(IDLE_MONITOR_PATH))
        .map_err(|e| format!("match rule: {e}"))?
        .build();
    let mut stream = zbus::MessageStream::for_match_rule(rule, &conn, None)
        .await
        .map_err(|e| format!("subscribe: {e}"))?;
    let mut watch_id: u32 = proxy
        .call("AddUserActiveWatch", &())
        .await
        .map_err(|e| format!("AddUserActiveWatch: {e}"))?;
    tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            let Ok(id) = msg.body().deserialize::<u32>() else {
                continue;
            };
            if id != watch_id {
                continue;
            }
            on_active();
            match proxy.call("AddUserActiveWatch", &()).await {
                Ok(new_id) => watch_id = new_id,
                Err(e) => {
                    tracing::warn!("user-active watch re-arm failed: {e}");
                    break;
                }
            }
        }
        tracing::warn!("user-active watch stream ended (session bus dropped?)");
    });
    Ok(())
}

/// Watch `LockedHint` changes on the graphical session and invoke
/// `on_change` with each fresh value. The caller uses this to push an
/// AppState immediately when the lock screen flips (a phone command OR a
/// local Super+L / lid event), instead of letting the phone's lock icon
/// go stale until the next periodic heartbeat. Runs until the bus
/// connection dies; returns Err only when the subscription can't be set
/// up at all.
pub async fn watch_locked_hint(
    on_change: impl Fn(bool) + Send + 'static,
) -> Result<(), String> {
    use futures::StreamExt;
    let conn = Connection::system()
        .await
        .map_err(|e| format!("system bus: {e}"))?;
    let path = graphical_session_path(&conn).await?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")
        .and_then(|b| b.member("PropertiesChanged"))
        .and_then(|b| b.path(path.clone()))
        .map_err(|e| format!("match rule: {e}"))?
        .build();
    let mut stream = zbus::MessageStream::for_match_rule(rule, &conn, None)
        .await
        .map_err(|e| format!("subscribe: {e}"))?;
    // Keep the connection alive for as long as the stream runs.
    tokio::spawn(async move {
        let _conn = conn;
        while let Some(Ok(msg)) = stream.next().await {
            let Ok((iface, changed, _invalidated)) = msg.body().deserialize::<(
                String,
                std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
                Vec<String>,
            )>() else {
                continue;
            };
            if iface != "org.freedesktop.login1.Session" {
                continue;
            }
            if let Some(v) = changed.get("LockedHint") {
                if let Ok(locked) = bool::try_from(v) {
                    on_change(locked);
                }
            }
        }
        tracing::warn!("locked-hint watch stream ended (system bus dropped?)");
    });
    Ok(())
}

/// Non-GNOME fallback for [`watch_user_active`]: poll [`idle_ms`] (~1.5 s) and
/// fire `on_active` on the RISING edge of activity — idle time collapsing from
/// "clearly idle" back to ~0. Cheap and good enough for the proximity-unlock
/// wake trigger; works wherever any [`idle_ms`] backend does (KDE / logind).
async fn watch_user_active_poll(on_active: impl Fn() + Send + 'static) -> Result<(), String> {
    if idle_ms().await.is_none() {
        return Err("no idle source available for user-active polling".to_string());
    }
    tracing::info!("user-active watch: polling idle fallback (non-GNOME desktop)");
    tokio::spawn(async move {
        let mut prev = idle_ms().await.unwrap_or(0);
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            let cur = idle_ms().await.unwrap_or(prev);
            // Rising edge: was clearly idle, now active again → user woke it.
            if prev >= 3000 && cur < 1500 {
                on_active();
            }
            prev = cur;
        }
    });
    Ok(())
}
