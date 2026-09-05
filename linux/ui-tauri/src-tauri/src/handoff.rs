//! Browsing HANDOFF consumer (laptop): a phone `HandoffEvent` → continue the
//! page here, continuity-style.
//!
//!  - `open_now = true`  (an explicit Share)         → open the URL right away.
//!  - `open_now = false` (the live accessibility read) → show a top-bar PILL
//!    badged with the SITE's domain + favicon; one click opens the page. An
//!    empty `url` clears it.
//!
//! The pill rides the live-activity pipeline (key `vortex-handoff`); the GNOME
//! extension renders it and, on click, opens the URL carried in `sub`. We only
//! ever put an http(s) URL on the pill.

use std::path::PathBuf;
use std::sync::Mutex;

use tauri::AppHandle;
use tokio::sync::mpsc::{self, UnboundedSender};

use vortex_l3_daemon::core::handoff::HandoffEvent;
use vortex_l3_daemon::core::icon_cache;
use vortex_l3_daemon::core::live_activity::LiveActivity;

/// Stable pill key for the browsing-handoff live activity.
const HANDOFF_PILL_KEY: &str = "vortex-handoff";

/// The URL currently on the pill — so an async favicon fetch only re-publishes
/// (to swap in the site icon) if the user is STILL on that page (didn't navigate
/// away while the favicon was downloading).
static CURRENT_URL: Mutex<String> = Mutex::new(String::new());

/// The handoff consumer's sender, so an AppState-carried handoff (the LAN
/// backstop) can be fed in alongside the dedicated BLE HANDOFF frame. Set once
/// at worker start.
pub(crate) static HANDOFF_TX: std::sync::OnceLock<UnboundedSender<HandoffEvent>> =
    std::sync::OnceLock::new();

/// LAN / BLE-STATE backstop: feed a peer AppState's `handoff` into the consumer.
/// Idempotent — the consumer dedups by URL, so a duplicate delivery just
/// refreshes (and heartbeats) the pill; an empty url clears it. The additive
/// path used when the dedicated BLE HANDOFF frame can't get through.
pub(crate) fn dispatch_appstate_handoff(handoff: &Option<HandoffEvent>) {
    if let (Some(ev), Some(tx)) = (handoff.as_ref(), HANDOFF_TX.get()) {
        let _ = tx.send(ev.clone());
    }
}

/// Spawn the handoff consumer; returns the sender the BLE listener feeds
/// `HandoffEvent`s into. `live_tx` publishes the "continue from phone" pill.
pub(crate) fn spawn_consumer(
    _app: AppHandle,
    live_tx: UnboundedSender<LiveActivity>,
) -> UnboundedSender<HandoffEvent> {
    let (tx, mut rx) = mpsc::unbounded_channel::<HandoffEvent>();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            // A handoff frame is proof of live phone contact (the 25s heartbeat
            // keeps this fresh while a page is up) → gates the disconnect-clear.
            crate::ble::touch_peer_contact();
            if ev.url.is_empty() {
                if let Ok(mut g) = CURRENT_URL.lock() {
                    g.clear();
                }
                let _ = live_tx.send(clear_pill());
                continue;
            }
            if !is_web_url(&ev.url) {
                tracing::warn!("handoff: ignoring non-http(s) url");
                continue;
            }
            if ev.open_now {
                open_url(&ev.url);
                continue;
            }
            // Live read → a "continue" pill badged with the site domain + icon.
            let domain = domain_of(&ev.url).unwrap_or_default();
            if let Ok(mut g) = CURRENT_URL.lock() {
                *g = ev.url.clone();
            }
            // Publish NOW with whatever icon is already cached (favicon from a
            // previous visit, else the phone browser icon) — no waiting.
            let cached = cached_favicon(&domain);
            let _ = live_tx.send(handoff_pill(&ev, &domain, cached.clone()));
            // If the favicon isn't cached yet, fetch it off-thread and re-publish
            // ONCE it lands — but only if we're still on this same page.
            if cached.is_none() && !domain.is_empty() {
                let live_tx = live_tx.clone();
                let ev = ev.clone();
                let domain2 = domain.clone();
                tokio::spawn(async move {
                    let id = tokio::task::spawn_blocking(move || ensure_favicon(&domain2))
                        .await
                        .ok()
                        .flatten();
                    if id.is_none() {
                        return;
                    }
                    let still_here =
                        CURRENT_URL.lock().map(|g| *g == ev.url).unwrap_or(false);
                    if still_here {
                        let d = domain_of(&ev.url).unwrap_or_default();
                        let _ = live_tx.send(handoff_pill(&ev, &d, id));
                    }
                });
            }
        }
    });
    tx
}

fn is_web_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// The host of an http(s) URL, `www.` stripped (e.g. "claude.ai"). None if
/// it can't be parsed.
fn domain_of(url: &str) -> Option<String> {
    let after = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    // The authority ends at the FIRST of `/`, `?` or `#`. Splitting on `/`
    // alone left the query string in, and `rsplit('@')` then treated anything
    // after a `@` in it as the host: `https://evil.com?r=@paypal.com` displayed
    // "paypal.com", with paypal's favicon, on a pill that opens evil.com.
    let authority = after
        .split(['/', '?', '#'])
        .next()?;
    let host = authority
        .rsplit('@')
        .next()? // strip any userinfo
        .split(':')
        .next()?; // strip port
    let host = host.strip_prefix("www.").unwrap_or(host);
    // A host is a domain, not a sentence: anything outside the character set a
    // hostname can contain means we could not parse this and should say so
    // rather than display something misleading.
    if host.is_empty()
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        None
    } else {
        Some(host.to_string())
    }
}

#[cfg(test)]
mod domain_tests {
    use super::domain_of;

    #[test]
    fn the_query_string_cannot_forge_the_host() {
        assert_eq!(domain_of("https://evil.com?r=@paypal.com").as_deref(), Some("evil.com"));
        assert_eq!(domain_of("https://evil.com/#@paypal.com").as_deref(), Some("evil.com"));
    }

    #[test]
    fn ordinary_urls_still_parse() {
        assert_eq!(domain_of("https://www.bbc.co.uk/news").as_deref(), Some("bbc.co.uk"));
        assert_eq!(domain_of("http://example.org:8080/x").as_deref(), Some("example.org"));
        assert_eq!(domain_of("https://user:pw@example.org/x").as_deref(), Some("example.org"));
        assert_eq!(domain_of("ftp://example.org"), None);
    }
}

/// The "continue from phone" pill. The clickable URL rides `sub`; the collapsed
/// label is the domain; the icon is the site favicon (falling back to the phone
/// browser's app icon).
fn handoff_pill(ev: &HandoffEvent, domain: &str, favicon_id: Option<String>) -> LiveActivity {
    // Card headline: prefer the page title, else the URL without its scheme.
    let headline = if ev.title.trim().is_empty() {
        ev.url
            .strip_prefix("https://")
            .or_else(|| ev.url.strip_prefix("http://"))
            .unwrap_or(&ev.url)
            .trim_end_matches('/')
            .to_string()
    } else {
        ev.title.clone()
    };
    // app_id drives the icon lookup: the favicon's synthetic id if we fetched it,
    // else the phone browser's package (its real icon), else nothing (generic).
    let app_id = favicon_id.unwrap_or_else(|| ev.app_id.clone());
    LiveActivity {
        key: HANDOFF_PILL_KEY.to_string(),
        app: domain.to_string(),
        app_id,
        title: headline,
        text: domain.to_string(), // collapsed pill label = the site
        sub: ev.url.clone(),      // ← the extension opens this on click
        progress: -1,
        started_at: 0,
        muted: false,
        speaker: false,
        has_earbuds: false,
        ended: false,
        playing: None,
    }
}

fn clear_pill() -> LiveActivity {
    LiveActivity {
        key: HANDOFF_PILL_KEY.to_string(),
        app: String::new(),
        app_id: String::new(),
        title: String::new(),
        text: String::new(),
        sub: String::new(),
        progress: -1,
        started_at: 0,
        muted: false,
        speaker: false,
        has_earbuds: false,
        ended: true,
        playing: None,
    }
}

/// Synthetic icon-cache key for a site favicon (so `icon_cache::icon_path`
/// resolves the pill icon to the fetched favicon).
fn favicon_app_id(domain: &str) -> String {
    format!("handoff_{domain}")
}

/// The synthetic app_id if this domain's favicon is ALREADY cached, else None.
fn cached_favicon(domain: &str) -> Option<String> {
    if domain.is_empty() {
        return None;
    }
    let app_id = favicon_app_id(domain);
    icon_cache::icon_path(&app_id)
        .filter(|p| p.exists())
        .map(|_| app_id)
}

/// Ensure the favicon for `domain` is cached as a PNG under the icon cache, and
/// return its synthetic app_id (for the pill). Fetches `https://<domain>/
/// favicon.ico` directly (privacy: the laptop fetches it like a browser would,
/// NOT via a third-party favicon service), decodes any format and re-encodes
/// PNG. Returns None on any failure (the pill then uses the browser icon).
fn ensure_favicon(domain: &str) -> Option<String> {
    let app_id = favicon_app_id(domain);
    let path: PathBuf = icon_cache::icon_path(&app_id)?;
    if path.exists() {
        return Some(app_id); // cached from a previous visit
    }
    let url = format!("https://{domain}/favicon.ico");
    let out = std::process::Command::new("curl")
        .args(["-sfL", "--max-time", "4", &url])
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let img = image::load_from_memory(&out.stdout).ok()?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    img.save_with_format(&path, image::ImageFormat::Png).ok()?;
    Some(app_id)
}

/// Open `url` in the default browser. The URL is never logged.
fn open_url(url: &str) {
    match std::process::Command::new("xdg-open").arg(url).spawn() {
        Ok(_) => tracing::info!("handoff: opened a shared page in the browser"),
        Err(e) => tracing::warn!("handoff: xdg-open failed: {e}"),
    }
}
