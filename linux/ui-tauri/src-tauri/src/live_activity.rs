//! Live Activities: the GNOME-extension D-Bus payload, and the per-app
//! tray-pill fallback when no extension is installed. Split out of lib.rs.

use std::sync::Arc;

use tauri::AppHandle;

use vortex_l3_daemon::core::live_activity::LiveActivity;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;

/// No phone frame over EITHER transport for this long ⇒ treat as a full
/// disconnect and clear every mirror pill (their buttons are dead without a
/// link). Sits above the slowest active-pill heartbeat (handoff, 25s) so a
/// normal inter-beat gap is never misread as a disconnect; well under the old
/// 90s staleness backstop.
const DISCONNECT_CLEAR_MS: u64 = 35_000;


/// Create / update / remove a live-activity's OWN tray icon (the Vortex tray
/// is left untouched). Each activity (by key) owns one tray: the source app's
/// real logo + a short status label, plus a click-menu listing the stages
/// (status / ETA / progress). MUST be called on the gtk main thread — tray and
/// menu objects are gtk-thread-bound on Linux.
/// True when the optional Vortex GNOME Shell extension is enabled — then IT
/// draws the live-activity cards and the daemon suppresses its tray fallback.
pub(crate) fn vortex_extension_enabled() -> bool {
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.shell", "enabled-extensions"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("vortex-live@vortex"))
        .unwrap_or(false)
}

/// JSON the GNOME extension reads: the active live activities + each one's
/// resolved icon path.
pub(crate) fn live_activities_json(
    active: &std::collections::HashMap<String, vortex_l3_daemon::core::live_activity::LiveActivity>,
) -> String {
    let arr: Vec<serde_json::Value> = active
        .values()
        .map(|la| {
            let icon = vortex_l3_daemon::core::icon_cache::icon_path(&la.app_id)
                .filter(|p| p.exists())
                .or_else(vortex_l3_daemon::core::icon_cache::ensure_generic)
                .and_then(|p| p.to_str().map(str::to_string))
                .unwrap_or_default();
            let mut v = serde_json::json!({
                "key": la.key, "app": la.app, "app_id": la.app_id,
                "title": la.title, "text": la.text, "sub": la.sub,
                "progress": la.progress, "icon": icon,
                "started_at": la.started_at,
                "muted": la.muted, "speaker": la.speaker, "has_earbuds": la.has_earbuds,
            });
            // Now-playing pill: presence of `playing` is what makes the
            // extension draw the transport buttons — omit it otherwise.
            if let Some(playing) = la.playing {
                v["playing"] = serde_json::json!(playing);
            }
            v
        })
        .collect();
    serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string())
}

/// Stable tray id for a live activity's key (same across its updates).
pub(crate) fn live_tray_id(key: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    format!("vortex-live-{:x}", h.finish())
}

pub(crate) fn update_live_tray(
    app: &AppHandle,
    live: &vortex_l3_daemon::core::live_activity::LiveActivity,
) {
    
    let id = live_tray_id(&live.key);
    if live.ended {
        // HIDE rather than remove: GNOME's appindicator support doesn't
        // reliably re-show a tray that was removed and later re-added with the
        // same object path, so we keep the object and toggle visibility.
        if let Some(tray) = app.tray_by_id(&id) {
            let _ = tray.set_visible(false);
        }
        return;
    }
    // Short label beside the icon: the detail line (ETA), else the status.
    let label: String = {
        let body = if !live.text.is_empty() {
            live.text.as_str()
        } else if !live.title.is_empty() {
            live.title.as_str()
        } else {
            live.app.as_str()
        };
        body.chars().take(28).collect()
    };
    // The app's cached logo, else the bundled generic bell.
    let icon = vortex_l3_daemon::core::icon_cache::icon_path(&live.app_id)
        .filter(|p| p.exists())
        .or_else(vortex_l3_daemon::core::icon_cache::ensure_generic)
        .and_then(|p| tauri::image::Image::from_path(p).ok());
    // Display-only menu listing the stages.
    let mut lines: Vec<String> = Vec::new();
    if !live.app.is_empty() {
        lines.push(live.app.clone());
    }
    if !live.title.is_empty() {
        lines.push(live.title.clone());
    }
    if !live.text.is_empty() {
        lines.push(live.text.clone());
    }
    if !live.sub.is_empty() {
        lines.push(live.sub.clone());
    }
    if live.progress >= 0 {
        let p = live.progress.clamp(0, 100);
        let filled = (p * 10 / 100) as usize;
        let bar: String = std::iter::repeat('▰')
            .take(filled)
            .chain(std::iter::repeat('▱').take(10 - filled))
            .collect();
        lines.push(format!("{bar}  {p}%"));
    }
    // Full info on hover — guarantees the text is reachable on EVERY desktop.
    // GNOME shows the inline title beside the icon, but KDE / Hyprland (waybar)
    // render the SNI title only as a tooltip, so without this the text would be
    // GNOME-only. The icon shows everywhere regardless; this gives text parity.
    let tooltip = lines.join("\n");
    let items: Vec<MenuItem<tauri::Wry>> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            MenuItem::with_id(app, format!("live-{id}-{i}"), t, false, None::<&str>).ok()
        })
        .collect();
    let item_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        items.iter().map(|i| i as &dyn tauri::menu::IsMenuItem<tauri::Wry>).collect();
    let menu = Menu::with_items(app, &item_refs).ok();

    if let Some(tray) = app.tray_by_id(&id) {
        if let Some(icon) = icon {
            let _ = tray.set_icon(Some(icon));
        }
        let _ = tray.set_title(Some(label.as_str()));
        let _ = tray.set_tooltip(Some(tooltip.as_str()));
        if let Some(menu) = menu {
            let _ = tray.set_menu(Some(menu));
        }
        let _ = tray.set_visible(true); // un-hide if a prior activity hid it
    } else {
        let mut b = TrayIconBuilder::with_id(&id)
            .title(label.as_str())
            .tooltip(tooltip.as_str());
        if let Some(icon) = icon {
            b = b.icon(icon);
        }
        if let Some(menu) = menu.as_ref() {
            b = b.menu(menu);
        }
        let _ = b.build(app);
    }
}

pub(crate) async fn spawn_consumer(
    app: AppHandle,
    call_action_tx: tokio::sync::mpsc::UnboundedSender<String>,
) -> tokio::sync::mpsc::UnboundedSender<LiveActivity> {
            let (ble_live_tx, mut ble_live_rx) = tokio::sync::mpsc::unbounded_channel::<
                vortex_l3_daemon::core::live_activity::LiveActivity,
            >();
            {
                use vortex_l3_daemon::core::live_activity::LiveActivity;
                let app = app.clone();
                // Publish live activities on D-Bus for the OPTIONAL GNOME extension.
                // The extension calls back via CallAction for in-call-pill buttons.
                // The GNOME shell extension's D-Bus service. `None` elsewhere,
                // which is also what a Linux desktop without the extension
                // gets — the tray fallback below draws the cards instead.
                #[cfg(target_os = "linux")]
                let live_dbus_tx =
                    vortex_l3_daemon::core::live_activity_dbus::start(call_action_tx).await.ok();
                #[cfg(not(target_os = "linux"))]
                let live_dbus_tx: Option<tokio::sync::mpsc::UnboundedSender<String>> = None;
                // If that extension is enabled it draws the cards, so suppress the
                // tray fallback (avoid showing both). Checked once at startup.
                let ext_enabled = vortex_extension_enabled();
                if ext_enabled {
                    tracing::info!("vortex GNOME extension enabled → live trays suppressed");
                }
                // Active live activities (for the D-Bus JSON) + last-seen time (for
                // the staleness sweeper: nav ended but onNotificationRemoved didn't
                // fire — e.g. a MIUI force-stop — so a stale entry can't linger).
                let active: Arc<tokio::sync::Mutex<std::collections::HashMap<String, LiveActivity>>> =
                    Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
                let live_last: Arc<
                    tokio::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
                > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
                {
                    let app = app.clone();
                    let live_last = live_last.clone();
                    let active = active.clone();
                    let dbus = live_dbus_tx.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            // A confirmed full disconnect (no frame over EITHER
                            // transport for a while) clears ALL mirror pills at
                            // once — their buttons (Accept / Mute / open) are dead
                            // without a link, so a lingering pill misleads. On
                            // reconnect the state re-syncs and the pills rebuild.
                            // The threshold sits above the slowest active-pill
                            // heartbeat (handoff, 25s) so a normal inter-beat gap
                            // is never mistaken for a disconnect.
                            let disconnected =
                                crate::presence::peer_contact_age_ms() > DISCONNECT_CLEAR_MS;
                            let stale: Vec<String> = {
                                let m = live_last.lock().await;
                                m.iter()
                                    // Disconnected → clear all. Else the 90s
                                    // backstop reaps a pill whose `ended` was lost
                                    // (force-stop) while still connected.
                                    .filter(|(_, t)| {
                                        disconnected
                                            || t.elapsed()
                                                > std::time::Duration::from_secs(90)
                                    })
                                    .map(|(k, _)| k.clone())
                                    .collect()
                            };
                            for key in stale {
                                live_last.lock().await.remove(&key);
                                // A swept CALL pill must let the call consumer
                                // know it's off-screen, so a re-asserted active
                                // call (e.g. after a >90s reconnect) REBUILDS it
                                // instead of being deduped as "already shown".
                                if key == crate::call::CALL_PILL_KEY {
                                    crate::call::CALL_PILL_ACTIVE
                                        .store(false, std::sync::atomic::Ordering::SeqCst);
                                }
                                {
                                    let mut a = active.lock().await;
                                    a.remove(&key);
                                    if let Some(tx) = &dbus {
                                        let _ = tx.send(live_activities_json(&a));
                                    }
                                }
                                if !ext_enabled {
                                    let app2 = app.clone();
                                    let id = live_tray_id(&key);
                                    let _ = app.run_on_main_thread(move || {
                                    
                                        if let Some(tray) = app2.tray_by_id(&id) {
                                            let _ = tray.set_visible(false);
                                        }
                                    });
                                }
                                tracing::info!(
                                    key = %key,
                                    reason = if disconnected { "disconnected" } else { "stale" },
                                    "live activity pill cleared",
                                );
                            }
                        }
                    });
                }
                // Each live activity → D-Bus (extension card) AND, when the
                // extension isn't enabled, its OWN tray icon (logo + status + stage
                // menu). Tray ops MUST run on the gtk main thread on Linux.
                tokio::spawn(async move {
                    while let Some(live) = ble_live_rx.recv().await {
                        tracing::info!(app = %live.app, ended = live.ended, "live activity update");
                        {
                            let mut a = active.lock().await;
                            if live.ended {
                                a.remove(&live.key);
                                live_last.lock().await.remove(&live.key);
                            } else {
                                a.insert(live.key.clone(), live.clone());
                                live_last
                                    .lock()
                                    .await
                                    .insert(live.key.clone(), std::time::Instant::now());
                            }
                            if let Some(tx) = &live_dbus_tx {
                                let _ = tx.send(live_activities_json(&a));
                            }
                        }
                        if !ext_enabled {
                            let app2 = app.clone();
                            let _ = app.run_on_main_thread(move || {
                                update_live_tray(&app2, &live);
                            });
                        }
                    }
                });
            }
    ble_live_tx
}
