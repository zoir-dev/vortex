//! Browsing HANDOFF (phone → laptop), seamless-continuity style: the page the
//! phone is on, to continue on the laptop. Rides the Noise-sealed AUDIO_SIGNAL
//! stream as frame `ty::HANDOFF`. Mirrors the Kotlin `HandoffEvent`.

use serde::{Deserialize, Serialize};

/// A page the phone wants to hand off to the laptop.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandoffEvent {
    /// The page URL. Empty = "stop handing off" (clears the laptop pill).
    pub url: String,
    /// Page / tab title for the pill label (may be empty).
    #[serde(default)]
    pub title: String,
    /// The source app's package id (e.g. `com.android.chrome`) so the laptop
    /// can show its real icon on the pill. May be empty.
    #[serde(default)]
    pub app_id: String,
    /// `true` = an explicit Share → the laptop opens it IMMEDIATELY. `false` =
    /// the live accessibility read → the laptop shows a "continue from phone"
    /// pill the user clicks to open.
    #[serde(default)]
    pub open_now: bool,
    /// Identity of an `open_now` request, so the laptop opens it EXACTLY once.
    ///
    /// The event also rides the phone's AppState snapshot as a LAN/BLE-STATE
    /// backstop, and a snapshot is republished on every heartbeat. Without an
    /// identity the consumer cannot tell "the share I already opened" from
    /// "open this URL again", so it re-opened the browser every ~12s for as
    /// long as the phone app stayed up.
    ///
    /// Empty on the live-read path, and from phone builds predating this field;
    /// the consumer then falls back to deduping by URL.
    #[serde(default)]
    pub id: String,
}

impl HandoffEvent {
    pub fn from_json(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }

    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}
