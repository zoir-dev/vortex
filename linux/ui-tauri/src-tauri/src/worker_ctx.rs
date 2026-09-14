//! Shared handles the worker command-loop hands to each command handler.
//!
//! `run_worker` builds one `WorkerCtx` after its init block, then each
//! `UiCmd` arm delegates to a per-feature handler (`cmd_pairing`, `cmd_earbuds`,
//! `mirror`) taking `&WorkerCtx`. Every field is cheap to clone (handles /
//! `Arc`s), so handlers clone what they need into spawned tasks. This keeps
//! run_worker a thin dispatcher and each feature's command logic in its own
//! file — so the project can grow without run_worker growing.

use std::sync::Arc;

use tauri::AppHandle;

#[cfg(target_os = "linux")]
use vortex_l3_daemon::core::audio_lan_session::SessionWriterMap;
#[cfg(target_os = "linux")]
use vortex_l3_daemon::core::audio_orchestrator::SwitchOrchestrator;
use vortex_l3_daemon::core::identity::IdentityRecord;
use vortex_l3_daemon::core::storage::peers::PeerStore;

/// What the UI-command handlers need to act. The BlueZ adapter and the audio
/// hand-off handles are Linux-only, so the struct is narrower elsewhere and the
/// handlers that reach for them are gated with it.
pub(crate) struct WorkerCtx {
    pub app: AppHandle,
    #[cfg(target_os = "linux")]
    pub adapter: bluer::Adapter,
    pub identity: IdentityRecord,
    pub peer_store: Arc<dyn PeerStore>,
    #[cfg(target_os = "linux")]
    pub switch_orchestrator: Arc<SwitchOrchestrator>,
    #[cfg(target_os = "linux")]
    pub session_writers: SessionWriterMap,
}
