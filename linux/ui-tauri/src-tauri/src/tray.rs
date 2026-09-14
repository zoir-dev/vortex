//! The system-tray icon and menu — one API, two implementations.
//!
//! The two desktops disagree about what a tray even is, so this is a facade
//! rather than a shared implementation:
//!
//! * [`crate::tray_ksni`] speaks **StatusNotifierItem** over D-Bus directly.
//!   That is what a modern Linux desktop actually consumes, and speaking it
//!   ourselves is what makes the menu render — with live battery rows — on
//!   hosts where Tauri's own tray came out blank.
//! * [`crate::tray_tauri`] uses **Tauri's `TrayIconBuilder`**, which is the
//!   Windows notification area. There is no StatusNotifierItem there to talk
//!   to, and no D-Bus to talk over.
//!
//! Both expose exactly [`setup`] and [`update_battery_rows`], so nothing above
//! this line knows which one it is talking to.
//!
//! The icon differs too, and deliberately: Linux gets a white monochrome glyph
//! because the top bar is dark even in light mode and SNI hosts do not recolor,
//! while Windows gets the full-colour brand icon because its taskbar is light
//! by default and does not recolor either. Each implementation carries its own.

#[cfg(target_os = "linux")]
pub(crate) use crate::tray_ksni::{setup, update_battery_rows};
#[cfg(not(target_os = "linux"))]
pub(crate) use crate::tray_tauri::{setup, update_battery_rows};
