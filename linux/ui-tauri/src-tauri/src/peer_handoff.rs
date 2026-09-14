//! Inbound `PEER_HANDOFF` handling, and the fan-out for the generic
//! additive-frame channel.
//!
//! The BLE listener forwards every allowed frame that has no dedicated handler
//! down ONE generic channel as `(peer_pub, frame_ty, payload)`. That channel is
//! single-consumer, and notes/todos already owned it — so a second additive
//! feature had nowhere to listen. Rather than add yet another parameter to
//! `run_listener` (which the channel's own doc comment exists to avoid), this
//! module owns the channel, handles the frames it cares about, and forwards
//! everything else on to notes.
//!
//! What arriving `RELEASE` means: the phone has made a *different* laptop its
//! active peer. Ownership on our side has to follow, or the UI keeps claiming
//! "Connected" to a phone that has moved on — the stale-card problem in design
//! doc §D4. We learn it immediately instead of on next contact.

use std::sync::Arc;

use tauri::AppHandle;
use vortex_l3_daemon::core::ble::frame::{sub, ty};
use vortex_l3_daemon::core::storage::peers::PeerStore;

/// Own the generic additive-frame channel: handle `PEER_HANDOFF`, forward the
/// rest to `notes_tx`. Returns the sender the BLE listener writes into.
pub(crate) fn spawn_dispatcher(
    app: AppHandle,
    peer_store: Arc<dyn PeerStore>,
    notes_tx: tokio::sync::mpsc::UnboundedSender<vortex_l3_daemon::core::ble::frame::RawFrame>,
) -> tokio::sync::mpsc::UnboundedSender<vortex_l3_daemon::core::ble::frame::RawFrame> {
    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<vortex_l3_daemon::core::ble::frame::RawFrame>();
    tokio::spawn(async move {
        while let Some(f) = rx.recv().await {
            if f.ty == ty::PEER_HANDOFF {
                handle(&app, &peer_store, f.peer_pub, &f.payload).await;
                continue;
            }
            // Filesystem ops (both an inbound request to serve and a reply to
            // one of ours) go to their own module. Routed here rather than
            // downstream of notes so a large READ reply never queues behind a
            // notes merge.
            if matches!(
                f.ty,
                ty::FS_REQ | ty::FS_META | ty::FS_DATA | ty::FS_ERR
            ) {
                crate::fs_link::dispatch(f);
                continue;
            }
            // Not ours — pass it along. A closed notes channel means the
            // app is shutting down; stop rather than spin.
            if notes_tx.send(f).is_err() {
                break;
            }
        }
    });
    tx
}

async fn handle(
    app: &AppHandle,
    peer_store: &Arc<dyn PeerStore>,
    peer_pub: [u8; 32],
    payload: &[u8],
) {
    // The kind is the first payload byte (the sender's writer only carries a
    // frame type). An empty payload is malformed: drop it rather than default
    // to a kind and act on a guess.
    let Some((&kind, rest)) = payload.split_first() else {
        tracing::warn!("PEER_HANDOFF with empty payload; ignoring");
        return;
    };
    match kind {
        sub::HANDOFF_RELEASE => {
            // Peer-supplied text, so sanitise before it can reach the UI or
            // the logs — same rule the pairing name path follows.
            let successor = String::from_utf8_lossy(rest).to_string();
            let successor =
                vortex_l3_daemon::core::pairing::handshake::sanitize_peer_name(&successor);
            tracing::info!(
                peer = %hex::encode(&peer_pub[..4]),
                successor = %if successor.is_empty() { "<unknown>".into() } else { successor.clone() },
                "peer released us — dropping active ownership"
            );
            // Only ownership is dropped, NOT trust: the phone still trusts us
            // and may well come back. Forget is a separate, user-driven act.
            crate::arbiter::release(&peer_pub);
            // Blank the pages that were showing this phone's data, then re-emit
            // so the UI's `active` flag clears on its card.
            crate::cmd_pairing::purge_peer_cache(app);
            crate::emit_peers(app, peer_store.clone()).await;
        }
        sub::HANDOFF_BUSY => {
            // Nothing sends this yet; log rather than drop silently so an
            // unexpected one is visible in the field.
            tracing::info!(
                peer = %hex::encode(&peer_pub[..4]),
                "PEER_HANDOFF BUSY — another peer holds that phone"
            );
        }
        sub::HANDOFF_CLAIM => {
            tracing::info!(
                peer = %hex::encode(&peer_pub[..4]),
                "PEER_HANDOFF CLAIM — no handler yet"
            );
        }
        other => tracing::warn!("PEER_HANDOFF unknown kind 0x{other:02x}; ignoring"),
    }
}
