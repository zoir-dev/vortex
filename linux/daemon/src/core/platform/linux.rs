//! Linux implementations of the platform seam.
//!
//! These delegate to the modules that already existed — the seam is a boundary,
//! not a rewrite, so behaviour on Linux is unchanged by construction.

use std::path::{Path, PathBuf};

use super::{BoxFuture, Notifier, SessionControl, UserPaths};

pub struct LinuxPaths;

impl UserPaths for LinuxPaths {
    /// The real XDG download directory (`~/Téléchargements` on a French
    /// desktop), never a hardcoded English `~/Downloads` — that mistake
    /// silently created a second folder beside the real one and filed every
    /// received file where the user never looks.
    fn downloads(&self) -> Option<PathBuf> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        Some(xdg_download_dir(&home).unwrap_or_else(|| home.join("Downloads")))
    }

    fn config(&self) -> Option<PathBuf> {
        Some(config_home()?.join("vortex"))
    }

    fn cache(&self) -> Option<PathBuf> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".cache"));
        Some(base.join("vortex"))
    }

    /// The journal, in practice — this answers for completeness, and points at
    /// the cache root so a file written here is never mistaken for state.
    fn logs(&self) -> Option<PathBuf> {
        self.cache()
    }
}

fn config_home() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    Some(
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config")),
    )
}

/// `XDG_DOWNLOAD_DIR` from the environment, else from the `user-dirs.dirs` file
/// `xdg-user-dir(1)` reads. Not required to exist — a configured-but-missing
/// folder is still the user's stated intent, and the caller creates it.
fn xdg_download_dir(home: &Path) -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("XDG_DOWNLOAD_DIR") {
        if let Some(p) = expand_home(&v.to_string_lossy(), home) {
            return Some(p);
        }
    }
    let text = std::fs::read_to_string(config_home()?.join("user-dirs.dirs")).ok()?;
    expand_home(&parse_user_dirs(&text, "XDG_DOWNLOAD_DIR")?, home)
}

/// Pull one key out of a `user-dirs.dirs` file: shell syntax, `# comment` lines
/// and `KEY="value"` assignments, last assignment winning as a shell would.
fn parse_user_dirs(text: &str, key: &str) -> Option<String> {
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(v);
        if !v.is_empty() {
            found = Some(v.to_string());
        }
    }
    found
}

/// Expand the `$HOME/…` (or `~/…`) prefix the spec mandates. Anything else must
/// already be absolute — a bare relative path is malformed, and guessing could
/// scatter files into the process's cwd.
fn expand_home(raw: &str, home: &Path) -> Option<PathBuf> {
    let raw = raw.trim();
    for prefix in ["$HOME", "${HOME}", "~"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            let rest = rest.trim_start_matches('/');
            return Some(if rest.is_empty() {
                home.to_path_buf()
            } else {
                home.join(rest)
            });
        }
    }
    let p = PathBuf::from(raw);
    p.is_absolute().then_some(p)
}

pub struct LinuxNotifier;

impl Notifier for LinuxNotifier {
    fn show(
        &self,
        summary: &str,
        body: &str,
        app_id: &str,
        actions: &[(String, String)],
        replaces: u32,
        urgent: bool,
    ) -> BoxFuture<Result<u32, String>> {
        let (summary, body, app_id) = (summary.to_string(), body.to_string(), app_id.to_string());
        let actions = actions.to_vec();
        Box::pin(async move {
            crate::core::notification_display::show_call_banner(
                &summary, &body, &app_id, &actions, replaces, urgent,
            )
            .await
        })
    }

    fn close(&self, id: u32) -> BoxFuture<Result<(), String>> {
        Box::pin(async move { crate::core::notification_display::close(id).await })
    }

    fn actions(&self, tx: tokio::sync::mpsc::UnboundedSender<(u32, String)>) {
        tokio::spawn(crate::core::notification_display::watch_actions(tx));
    }

    fn closures(&self, tx: tokio::sync::mpsc::UnboundedSender<(u32, u32)>) {
        tokio::spawn(crate::core::notification_display::watch_closed(tx));
    }
}

pub struct LinuxSession;

impl SessionControl for LinuxSession {
    fn lock(&self) -> BoxFuture<Result<(), String>> {
        Box::pin(crate::core::session_lock::lock())
    }

    fn unlock(&self) -> BoxFuture<Result<(), String>> {
        Box::pin(crate::core::session_lock::unlock())
    }

    fn is_locked(&self) -> BoxFuture<Option<bool>> {
        Box::pin(crate::core::session_lock::locked_hint())
    }

    /// logind can unlock, given the one-time polkit rule.
    fn can_unlock(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real French `user-dirs.dirs` — the case that made this code necessary.
    const FR: &str = r#"# This file is written by xdg-user-dirs-update
XDG_DESKTOP_DIR="$HOME/Bureau"
XDG_DOWNLOAD_DIR="$HOME/Téléchargements"
XDG_DOCUMENTS_DIR="$HOME/Documents"
"#;

    #[test]
    fn parses_localised_download_dir() {
        let raw = parse_user_dirs(FR, "XDG_DOWNLOAD_DIR").expect("download dir");
        assert_eq!(raw, "$HOME/Téléchargements");
        assert_eq!(
            expand_home(&raw, Path::new("/home/cyril")),
            Some(PathBuf::from("/home/cyril/Téléchargements"))
        );
    }

    #[test]
    fn ignores_comments_and_other_keys() {
        assert_eq!(parse_user_dirs(FR, "XDG_MUSIC_DIR"), None);
        let text = "#XDG_DOWNLOAD_DIR=\"$HOME/nope\"\nXDG_DOWNLOAD_DIR=\"$HOME/yes\"\n";
        assert_eq!(
            parse_user_dirs(text, "XDG_DOWNLOAD_DIR"),
            Some("$HOME/yes".to_string())
        );
    }

    #[test]
    fn last_assignment_wins_like_a_shell() {
        let text = "XDG_DOWNLOAD_DIR=\"$HOME/first\"\nXDG_DOWNLOAD_DIR=\"$HOME/second\"\n";
        assert_eq!(
            parse_user_dirs(text, "XDG_DOWNLOAD_DIR"),
            Some("$HOME/second".to_string())
        );
    }

    #[test]
    fn expands_home_forms_and_rejects_relative() {
        let home = Path::new("/home/cyril");
        for raw in ["$HOME/Dl", "${HOME}/Dl", "~/Dl"] {
            assert_eq!(expand_home(raw, home), Some(PathBuf::from("/home/cyril/Dl")));
        }
        assert_eq!(expand_home("$HOME/", home), Some(home.to_path_buf()));
        assert_eq!(expand_home("/data/dl", home), Some(PathBuf::from("/data/dl")));
        assert_eq!(expand_home("Downloads", home), None);
        assert_eq!(expand_home("", home), None);
    }

    #[test]
    fn handles_unquoted_and_single_quoted() {
        assert_eq!(
            parse_user_dirs("XDG_DOWNLOAD_DIR=$HOME/Dl\n", "XDG_DOWNLOAD_DIR"),
            Some("$HOME/Dl".to_string())
        );
        assert_eq!(
            parse_user_dirs("XDG_DOWNLOAD_DIR='$HOME/Dl'\n", "XDG_DOWNLOAD_DIR"),
            Some("$HOME/Dl".to_string())
        );
    }
}


// ---------------------------------------------------------------------------
// BLE central over BlueZ
// ---------------------------------------------------------------------------

use std::sync::Arc;

use super::{AdvCandidate, AudioHandoff, BleCentral, GattLink, PeerAddr, Uuid128};
use crate::core::ble::client::VortexClient;
use crate::core::ble::{
    AUDIO_SIGNAL_UUID, CAPABILITY_UUID, PAIRING_CONTROL_UUID, RECONNECT_CONTROL_UUID,
};

/// BlueZ-backed [`BleCentral`]. Wraps the existing [`VortexClient`] and scanner
/// rather than reimplementing them: everything hard-won about connecting to a
/// dual-mode phone (see the bearer-selection comment in `ble::client`) stays in
/// one place, and this file only adapts the shapes.
pub struct LinuxBleCentral {
    adapter: bluer::Adapter,
}

impl LinuxBleCentral {
    /// Takes the process's shared adapter — see the note on [`BleCentral`] for
    /// why this is passed in rather than acquired here.
    pub fn new(adapter: bluer::Adapter) -> Self {
        Self { adapter }
    }
}

impl BleCentral for LinuxBleCentral {
    /// First Vortex advertisement seen, or `None` on timeout.
    ///
    /// `run_filtered_scan` never returns on its own — it is meant to be driven
    /// until dropped — so it races against the deadline and the first hit.
    fn scan_for_peer(&self, timeout_ms: u64) -> BoxFuture<Result<Option<AdvCandidate>, String>> {
        let adapter = self.adapter.clone();
        Box::pin(async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AdvCandidate>();
            let scan = crate::core::ble::scanner::run_filtered_scan(adapter, move |c| {
                // The scanner has already run the §5.2 filter, so the payload
                // it hands over is valid — pass it through rather than
                // re-deriving anything from the address.
                let _ = tx.send(AdvCandidate {
                    addr: PeerAddr(c.address.0),
                    payload: c.payload,
                    rssi: c.rssi,
                    local_name: c.local_name.clone(),
                });
            });
            tokio::select! {
                // Dropping `scan` here stops the discovery, which is the
                // documented way to end it.
                Some(found) = rx.recv() => Ok(Some(found)),
                r = scan => match r {
                    // It only returns on error; a clean return means the
                    // discovery ended without a candidate.
                    Ok(()) => Ok(None),
                    Err(e) => Err(format!("scan: {e}")),
                },
                _ = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)) => Ok(None),
            }
        })
    }

    fn connect(&self, addr: PeerAddr) -> BoxFuture<Result<Box<dyn GattLink>, String>> {
        let adapter = self.adapter.clone();
        Box::pin(async move {
            let address = bluer::Address::new(addr.0);
            let client = VortexClient::connect(&adapter, address)
                .await
                .map_err(|e| format!("connect {address}: {e}"))?;
            Ok(Box::new(LinuxGattLink::from_client(adapter, &client)) as Box<dyn GattLink>)
        })
    }

    /// Paired devices known to the adapter.
    ///
    /// A device that fails to answer `is_paired` is skipped rather than
    /// failing the list: one stale record must not hide the rest.
    fn bonded(&self) -> BoxFuture<Result<Vec<PeerAddr>, String>> {
        let adapter = self.adapter.clone();
        Box::pin(async move {
            let addrs = adapter
                .device_addresses()
                .await
                .map_err(|e| format!("device_addresses: {e}"))?;
            let mut out = Vec::new();
            for a in addrs {
                let Ok(dev) = adapter.device(a) else { continue };
                if dev.is_paired().await.unwrap_or(false) {
                    out.push(PeerAddr(a.0));
                }
            }
            Ok(out)
        })
    }

    fn adapter_ready(&self) -> BoxFuture<bool> {
        let adapter = self.adapter.clone();
        Box::pin(async move { adapter.is_powered().await.unwrap_or(false) })
    }
}

/// An open BlueZ GATT link: the characteristics a [`VortexClient`] resolved,
/// plus the adapter and address needed to answer "are we still connected?".
///
/// Holds the characteristics rather than the client so that [`Self::from_client`]
/// can BORROW a client the caller keeps using. bluer's handles are cheap clones
/// of D-Bus paths, and the existing call sites (pairing, the BLE loop) still
/// want their `VortexClient` for the typed helpers after the handshake.
pub struct LinuxGattLink {
    adapter: bluer::Adapter,
    address: bluer::Address,
    capability: bluer::gatt::remote::Characteristic,
    pairing_control: bluer::gatt::remote::Characteristic,
    reconnect_control: bluer::gatt::remote::Characteristic,
    /// `None` on phone builds before P2.13 — see [`GattLink::has`].
    audio_signal: Option<bluer::gatt::remote::Characteristic>,
    /// One forwarding task per subscription, aborted on disconnect. bluer hands
    /// back a Stream; the seam hands out a channel, so something has to pump.
    ///
    /// `Arc` because the returned futures are `'static` (the trait's `BoxFuture`
    /// carries no lifetime), so they cannot borrow from `&self` — they get a
    /// handle instead.
    notify_tasks: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl LinuxGattLink {
    /// Present an already-connected [`VortexClient`] as a [`GattLink`].
    ///
    /// This is the migration path: the pairing and reconnect flows move onto
    /// `&dyn GattLink` while their callers keep the connect logic they have —
    /// all the dual-mode bearer handling in `ble::client` — and simply wrap it
    /// here. No second connect, no behaviour change.
    pub fn from_client(adapter: bluer::Adapter, client: &VortexClient) -> Self {
        Self {
            adapter,
            address: client.address,
            capability: client.capability.clone(),
            pairing_control: client.pairing_control.clone(),
            reconnect_control: client.reconnect_control.clone(),
            audio_signal: client.audio_signal.clone(),
            notify_tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

impl LinuxGattLink {
    /// Resolve a UUID to the characteristic the client already discovered.
    ///
    /// A match rather than a map because the set is fixed by the spec (§10.1)
    /// and `audio_signal` is optional — an absent one has to read as "not on
    /// this peer", not as a lookup bug.
    fn characteristic(
        &self,
        uuid: Uuid128,
    ) -> Result<&bluer::gatt::remote::Characteristic, String> {
        let u = uuid::Uuid::from_u128(uuid);
        if u == CAPABILITY_UUID {
            Ok(&self.capability)
        } else if u == PAIRING_CONTROL_UUID {
            Ok(&self.pairing_control)
        } else if u == RECONNECT_CONTROL_UUID {
            Ok(&self.reconnect_control)
        } else if u == AUDIO_SIGNAL_UUID {
            self.audio_signal
                .as_ref()
                .ok_or_else(|| "audio-signal characteristic absent on this peer".to_string())
        } else {
            Err(format!("{u} is not a vortex characteristic"))
        }
    }
}

impl GattLink for LinuxGattLink {
    fn write(
        &self,
        char_uuid: Uuid128,
        data: &[u8],
        with_response: bool,
    ) -> BoxFuture<Result<(), String>> {
        let c = self.characteristic(char_uuid).cloned();
        let bytes = data.to_vec();
        Box::pin(async move {
            let c = c?;
            let req = bluer::gatt::remote::CharacteristicWriteRequest {
                offset: 0,
                op_type: if with_response {
                    bluer::gatt::WriteOp::Request
                } else {
                    bluer::gatt::WriteOp::Command
                },
                prepare_authorize: false,
                ..Default::default()
            };
            c.write_ext(&bytes, &req)
                .await
                .map_err(|e| format!("gatt write: {e}"))
        })
    }

    fn read(&self, char_uuid: Uuid128) -> BoxFuture<Result<Vec<u8>, String>> {
        let c = self.characteristic(char_uuid).cloned();
        Box::pin(async move { c?.read().await.map_err(|e| format!("gatt read: {e}")) })
    }

    fn subscribe(
        &self,
        char_uuid: Uuid128,
        tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    ) -> BoxFuture<Result<(), String>> {
        let c = self.characteristic(char_uuid).cloned();
        let tasks = Arc::clone(&self.notify_tasks);
        Box::pin(async move {
            let c = c?;
            let stream = c.notify().await.map_err(|e| format!("gatt notify: {e}"))?;
            let handle = tokio::spawn(async move {
                use futures::StreamExt;
                let mut stream = std::pin::pin!(stream);
                while let Some(bytes) = stream.next().await {
                    if tx.send(bytes).is_err() {
                        break; // consumer gone
                    }
                }
            });
            if let Ok(mut g) = tasks.lock() {
                g.push(handle);
            }
            Ok(())
        })
    }

    fn peer(&self) -> PeerAddr {
        PeerAddr(self.address.0)
    }

    fn has(&self, char_uuid: Uuid128) -> bool {
        self.characteristic(char_uuid).is_ok()
    }

    /// Stop forwarding notifications and let the link go.
    ///
    /// Deliberately does NOT call `Device::disconnect()`. On a dual-mode phone
    /// that tears down every bearer, including the A2DP/HFP link if the phone
    /// is also paired as an audio device — so a "close this GATT link" would
    /// cut the user's music. BlueZ drops the LE link once the handles go, which
    /// is what the pre-seam code relied on too.
    fn disconnect(&self) -> BoxFuture<Result<(), String>> {
        let taken: Vec<tokio::task::JoinHandle<()>> = self
            .notify_tasks
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        Box::pin(async move {
            for t in taken {
                t.abort();
            }
            Ok(())
        })
    }

    fn is_connected(&self) -> BoxFuture<bool> {
        let adapter = self.adapter.clone();
        let address = self.address;
        Box::pin(async move {
            match adapter.device(address) {
                Ok(d) => d.is_connected().await.unwrap_or(false),
                Err(_) => false,
            }
        })
    }
}

/// Linux audio handoff: the PulseAudio/BlueZ switch orchestrator plus the MPRIS
/// store the fast-path pause needs. Both already existed; this only presents
/// them to the (platform-neutral) BLE event stream.
pub struct LinuxAudioHandoff {
    orchestrator: Arc<crate::core::audio_orchestrator::SwitchOrchestrator>,
    media_store: crate::core::media_runtime::MediaStateStore,
}

impl LinuxAudioHandoff {
    pub fn new(
        orchestrator: Arc<crate::core::audio_orchestrator::SwitchOrchestrator>,
        media_store: crate::core::media_runtime::MediaStateStore,
    ) -> Self {
        Self {
            orchestrator,
            media_store,
        }
    }
}

impl AudioHandoff for LinuxAudioHandoff {
    fn pause_for_call(&self) -> BoxFuture<()> {
        let store = self.media_store.clone();
        Box::pin(async move {
            let paused = crate::core::media_runtime::pause_playing_for_call(&store).await;
            if !paused.is_empty() {
                tracing::info!(?paused, "BLE fast-path: paused MPRIS for call");
            }
        })
    }

    fn on_incoming(
        &self,
        peer: [u8; 32],
        frame: crate::core::audio_op::AudioOpFrame,
    ) -> BoxFuture<()> {
        let orch = Arc::clone(&self.orchestrator);
        Box::pin(async move {
            let _ = orch.on_incoming(peer, frame).await;
        })
    }
}
