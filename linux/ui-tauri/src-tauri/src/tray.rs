//! System-tray icon + menu, spoken as StatusNotifierItem ourselves (ksni).
//!
//! NOT Tauri's tray, and the reason is the click. On Linux Tauri goes through
//! libayatana-appindicator, whose D-Bus item exposes no `Activate` method at
//! all — only `Scroll` and `SecondaryActivate`. GNOME's appindicator extension
//! calls `Activate` on a DOUBLE click and disables the gesture outright when
//! the method is missing (`supportsActivation === false`), so
//! double-click-to-open — what every other tray app does, Telegram included —
//! could not be made to work from the app side. `on_tray_icon_event` never
//! fires there either: tray-icon documents click events as unsupported on
//! Linux. Speaking the spec ourselves puts `Activate` back on the item, and
//! with it the double click (KDE and XFCE activate on a single click).
//!
//! `run()`'s setup hook calls [`setup`] once at startup.

use std::sync::{LazyLock, Mutex, OnceLock};

use ksni::{menu::StandardItem, Handle, Icon, MenuItem, ToolTip, Tray, TrayMethods};

use vortex_l3_daemon::core::appstate::{AppState, EarbudsInfo};

use crate::{CmdChannel, UiCmd};

/// White monochrome BRAND SPIRAL for the status area. Like Telegram / Cursor,
/// we ship ONE fixed light icon rather than swapping per theme: the
/// GNOME/Ubuntu top bar is dark even in light mode, and Linux SNI hosts render
/// the pixmap as-is (no auto-recolor), so a single white glyph reads
/// everywhere. Embedded via include_bytes so it works from the standalone prod
/// binary.
static ICON: LazyLock<Vec<Icon>> = LazyLock::new(|| {
    let Ok(img) = image::load_from_memory_with_format(
        include_bytes!("../icons/tray.png"),
        image::ImageFormat::Png,
    ) else {
        tracing::warn!("tray: icon failed to decode; falling back to a themed name");
        return Vec::new();
    };
    // Offer panel sizes rather than the 512px source: the host picks the one it
    // wants and the pixmap crosses D-Bus on every read, so a megabyte of ARGB
    // per icon change is a megabyte wasted. 64 covers a 1x panel, 128 a HiDPI
    // one.
    [64u32, 128]
        .iter()
        .map(|&s| {
            let scaled = img.resize_exact(s, s, image::imageops::FilterType::Lanczos3);
            let mut data = scaled.into_rgba8().into_vec();
            for px in data.chunks_exact_mut(4) {
                px.rotate_right(1); // RGBA → ARGB32, which is what the spec asks for
            }
            Icon { width: s as i32, height: s as i32, data }
        })
        .collect()
});

/// The live tray. Its fields ARE the rendered menu: ksni asks for `menu()`
/// again after every [`Handle::update`], so a battery change is a field write.
struct VortexTray {
    app: tauri::AppHandle,
    /// Battery readout rows, as plain WORD labels ("Buds"/"Phone") rather than
    /// emoji or item icons: SNI hosts render neither custom menu-item icons nor
    /// color emoji reliably — both come out blank on dark themes. Plain text
    /// renders in the theme color everywhere.
    buds: String,
    phone: String,
    tooltip: String,
}

impl Tray for VortexTray {
    fn id(&self) -> String {
        "vortex".into()
    }

    fn title(&self) -> String {
        "Vortex".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        ICON.clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.tooltip.clone(),
            ..Default::default()
        }
    }

    /// Double-click on GNOME, single click on KDE/XFCE.
    fn activate(&mut self, _x: i32, _y: i32) {
        crate::window::toggle_main(&self.app);
    }

    /// Middle click — the same thing, since the alternative is nothing at all.
    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        crate::window::toggle_main(&self.app);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: self.phone.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: self.buds.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Switch earbuds".into(),
                // Toggle the buds between this laptop and the phone. The
                // callback must not block — the menu is frozen until it
                // returns — so it only posts to the worker.
                activate: Box::new(|t: &mut Self| {
                    use tauri::Manager;
                    if let Some(ch) = t.app.try_state::<CmdChannel>() {
                        let _ = ch.0.send(UiCmd::ToggleEarbuds);
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Show".into(),
                activate: Box::new(|t: &mut Self| crate::window::present_main(&t.app)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Self| t.app.exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Set once the item is registered with the host; `None` until then, and
/// forever on a desktop with no StatusNotifierWatcher at all.
static TRAY: OnceLock<Handle<VortexTray>> = OnceLock::new();

/// Phone-side fields cached from the last inbound AppState, so a local-only
/// refresh (BlueZ rescan — knows nothing about the phone) can still redraw
/// both rows without wiping the phone's data.
struct PhoneSnap {
    name: Option<String>,
    battery: Option<u8>,
    charging: bool,
    earbuds: Option<EarbudsInfo>,
    /// When this snapshot arrived, so the tray can stop believing it.
    at: std::time::Instant,
}

/// How long a phone snapshot is worth rendering. The phone's own foreground
/// notification already ages its copy of the laptop's state out at this
/// threshold — the tray simply never did the same in the other direction, so a
/// phone switched off at night left the tray cheerfully reporting "Redmi 9
/// 67% ⚡" and "Buds 80% (phone)" until the app was next restarted.
const PHONE_SNAP_FRESH: std::time::Duration = std::time::Duration::from_secs(30);

static LAST_PHONE: Mutex<Option<PhoneSnap>> = Mutex::new(None);

/// Redraw the tray tooltip + the two battery menu rows. The single render
/// path for all three triggers: inbound phone state over LAN (lan.rs),
/// inbound phone state over BLE (lan_state.rs), and the UI's 5-second local
/// earbuds rescan (cmd_earbuds.rs). The last one is what makes the buds row
/// appear the moment they connect to the laptop, instead of sitting on
/// "Buds --" until the phone's next heartbeat happens to arrive.
pub(crate) fn update_battery_rows(
    local_earbuds: Option<&EarbudsInfo>,
    phone: Option<&AppState>,
) {
    // A fresh phone state refreshes the cache; a local-only refresh reuses it.
    let mut cache = LAST_PHONE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(p) = phone {
        *cache = Some(PhoneSnap {
            name: p.name.clone(),
            battery: p.battery,
            charging: p.charging,
            earbuds: p.earbuds.clone(),
            at: std::time::Instant::now(),
        });
    }
    // Stale is the same as absent: better an honest "--" than a battery
    // percentage from hours ago presented as current.
    if cache
        .as_ref()
        .is_some_and(|s| s.at.elapsed() > PHONE_SNAP_FRESH)
    {
        *cache = None;
    }
    let snap = &*cache;

    let pf = |v: Option<u8>| v.map(|x| format!("{x}%")).unwrap_or_else(|| "--".to_string());
    let trunc = |s: &str, max: usize| -> String {
        if s.chars().count() > max {
            let head: String = s.chars().take(max.saturating_sub(3)).collect();
            format!("{}...", head.trim_end())
        } else {
            s.to_string()
        }
    };

    let phone_buds = snap.as_ref().and_then(|s| s.earbuds.as_ref());
    let laptop_owns = local_earbuds.map(|e| e.connected).unwrap_or(false);
    let phone_has = phone_buds.map(|e| e.connected).unwrap_or(false);
    let buds_pct = if laptop_owns {
        local_earbuds.and_then(|e| e.battery)
    } else {
        phone_buds.and_then(|e| e.battery)
    };
    let owner = if laptop_owns {
        "laptop"
    } else if phone_has {
        "phone"
    } else {
        "—"
    };
    let tip = format!(
        "Vortex   🎧 {} ({})   📱 {}",
        pf(buds_pct),
        owner,
        pf(snap.as_ref().and_then(|s| s.battery))
    );
    let buds_name = if laptop_owns {
        local_earbuds.map(|e| e.name.clone())
    } else {
        phone_buds.map(|e| e.name.clone())
    }
    .filter(|n| !n.is_empty())
    .or_else(|| vortex_l3_daemon::core::earbuds_store::load().map(|s| s.name))
    .unwrap_or_else(|| "Buds".to_string());
    let buds_text = format!("{}   {} ({})", trunc(&buds_name, 18), pf(buds_pct), owner);
    // ⚡ (U+26A1, present in DejaVu Sans — portable) marks a charging device.
    // No phone seen yet this session → leave the row on its "Phone   --"
    // placeholder rather than inventing a name.
    let phone_text = snap.as_ref().map(|s| {
        let bolt = if s.charging { " \u{26A1}" } else { "" };
        let name = s
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "Phone".to_string());
        format!("{}   {}{}", trunc(&name, 18), pf(s.battery), bolt)
    });
    drop(cache);

    let Some(handle) = TRAY.get().cloned() else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        handle
            .update(move |t: &mut VortexTray| {
                t.buds = buds_text;
                t.tooltip = tip;
                if let Some(pt) = phone_text {
                    t.phone = pt;
                }
            })
            .await;
    });
}

pub(crate) fn setup(app: &tauri::App) -> tauri::Result<()> {
    let handle = app.handle().clone();
    // Registering with the host is a D-Bus round-trip, so it happens off the
    // setup hook. A desktop with no StatusNotifierWatcher (a bare wlroots
    // session, GNOME without the appindicator extension) simply has no tray —
    // the app keeps running headless in exactly the way it already did.
    tauri::async_runtime::spawn(async move {
        let tray = VortexTray {
            app: handle,
            buds: "Buds   --".into(),
            phone: "Phone   --".into(),
            tooltip: "Vortex".into(),
        };
        match tray.spawn().await {
            Ok(h) => {
                let _ = TRAY.set(h);
                tracing::info!("tray: StatusNotifierItem registered");
            }
            Err(e) => tracing::warn!("tray: no status-notifier host ({e}); running without a tray"),
        }
    });
    Ok(())
}
