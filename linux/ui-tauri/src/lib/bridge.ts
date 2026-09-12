import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface IdentityInfo {
  /** True once the daemon has loaded or generated the local identity. */
  ready: boolean;
}

export interface ScanHit {
  addr: string;
  rssi: number;
  instance: string;
  name: string | null;
}

export interface TrustedPeer {
  peer_static_pub: string;
  paired_at: number;
  peer_name?: string | null;
  /** True for the peer that currently owns the session (see `arbiter` on the
   *  backend). With several trusted phones, "remembered" and "the one whose
   *  data is on screen" are different things. */
  active: boolean;
}

/** A trusted peer found on air during a switch scan. */
export interface SwitchCandidate {
  peer_static_pub: string;
  name?: string | null;
  rssi: number;
}

/**
 * Peer's live app state, pushed by the daemon after every successful
 * post-handshake sync. Pairing security stays untouched; this is the
 * orthogonal JSON-over-AEAD channel.
 */
export interface PeerState {
  peer_static_pub: string;
  battery: number | null;
  class: "unknown" | "laptop" | "phone" | "tablet" | "earbuds";
  name: string | null;
  locale: string | null;
  theme: string | null;
  earbuds: { name: string; battery: number | null; connected: boolean } | null;
  charging: boolean;
  ts: number;
}

export interface PairingStartedEvent {
  peer_addr: string;
}

export type PairingResultEvent =
  | { ok: true; message: string }
  | { ok: false; error: string };

// Tauri commands — names must match #[tauri::command] in src-tauri/src/lib.rs.
export async function startScan(): Promise<void> {
  await invoke("start_scan");
}

export async function startPair(addr: string): Promise<void> {
  await invoke("start_pair", { addr });
}

/**
 * Approve or reject the current pairing session. The backend has
 * surfaced a `vortex:pairing_sas` event and is awaiting this call to
 * advance the dual-approval handshake. A double-click or a call with
 * no pairing in flight is a no-op on the backend side.
 */
export async function pairDecision(approve: boolean): Promise<void> {
  await invoke("pair_decision", { approve });
}

export async function forgetPeer(peerStaticPub: string): Promise<void> {
  await invoke("forget_peer", { peerStaticPub });
}

/**
 * "Switch device": keep the current phone connected and look for another
 * already-trusted one. Not a disconnect — the backend holds the active link
 * for the whole scan, so a cancelled or fruitless switch leaves you exactly
 * where you were.
 */
export async function switchPeer(): Promise<void> {
  await invoke("switch_peer");
}

export async function cancelSwitch(): Promise<void> {
  await invoke("cancel_switch");
}

/** Adopt this trusted peer as the active one (the user's pick, or the sole
 *  candidate found). */
export async function activatePeer(peerStaticPub: string): Promise<void> {
  await invoke("activate_peer", { peerStaticPub });
}

export async function forgetAll(): Promise<void> {
  await invoke("forget_all");
}

/**
 * Ask the worker to re-emit identity + peers. Call this on mount so the
 * UI is populated even if Vue subscribed after the worker's initial
 * emit (HMR reload, race on first paint, etc.).
 */
export async function refreshState(): Promise<void> {
  await invoke("refresh_state");
}

/**
 * Ask the worker to re-probe the local BlueZ stack for connected
 * earbuds and emit a fresh `vortex:local_earbuds` event. Called on
 * mount + on a short interval so the UI reflects the user pulling
 * an earbud out without waiting for the next peer reconnect.
 */
export async function refreshLocalEarbuds(): Promise<void> {
  await invoke("refresh_local_earbuds");
}

/**
 * Settings toggle: enable/disable smart audio-follow on this laptop. When
 * off, the laptop never auto-grabs the buds on a local media play-edge
 * (manual tray switch still works). Best-effort — a call before the media
 * watcher has started is a backend no-op (see `syncSmartSwitchToDaemon`).
 */
export async function setSmartSwitchEnabled(enabled: boolean): Promise<void> {
  await invoke("set_smart_switch_enabled", { enabled });
}

/** Current smart-audio-follow runtime state (daemon is the source of truth). */
export async function getSmartSwitchEnabled(): Promise<boolean> {
  return await invoke<boolean>("get_smart_switch_enabled");
}

/** Proximity auto-lock/unlock toggles (laptop-local, persisted). */
export interface ProximitySettings {
  auto_lock: boolean;
  auto_unlock: boolean;
}

export async function getProximitySettings(): Promise<ProximitySettings> {
  return await invoke<ProximitySettings>("get_proximity_settings");
}

export async function setProximitySettings(s: ProximitySettings): Promise<void> {
  await invoke("set_proximity_settings", {
    autoLock: s.auto_lock,
    autoUnlock: s.auto_unlock,
  });
}

/**
 * Settings toggle: whether THIS laptop shows mirrored phone notifications.
 * Per-device + local — not synced to the phone (each side controls what it
 * displays). Off = incoming notification frames are dropped silently.
 */
export async function setNotifMirrorShow(show: boolean): Promise<void> {
  await invoke("set_notif_mirror_show", { show });
}

/** Current laptop notification-display state. */
export async function getNotifMirrorShow(): Promise<boolean> {
  return await invoke<boolean>("get_notif_mirror_show");
}

/** Whether THIS laptop sends its own desktop notifications to the phone. */
export async function setNotifMirrorSend(send: boolean): Promise<void> {
  await invoke("set_notif_mirror_send", { send });
}

export async function getNotifMirrorSend(): Promise<boolean> {
  return await invoke<boolean>("get_notif_mirror_send");
}

/**
 * One row in the in-app BT picker. `is_audio=true` flags devices
 * advertising A2DP/HFP/HSP so the UI can sort them to the top.
 */
export interface BluetoothDeviceRow {
  address: string;
  name: string;
  rssi: number | null;
  connected: boolean;
  is_audio: boolean;
}

/**
 * Run a short BlueZ discovery and return everything the OS knows
 * about — paired devices plus anything that showed up while the
 * scan was running. The Vue layer renders this in the
 * "+ Add earbuds" modal.
 */
export async function scanBluetoothDevices(): Promise<BluetoothDeviceRow[]> {
  return await invoke<BluetoothDeviceRow[]>("scan_bluetooth_devices");
}

/** Persist the user's earbud choice so the card always shows. */
export async function saveEarbuds(address: string, name: string): Promise<void> {
  await invoke("save_earbuds", { address, name });
}

/** Forget the saved earbud (the card returns to the "+" state). */
export async function clearEarbuds(): Promise<void> {
  await invoke("clear_earbuds");
}

/** The on-disk saved earbuds row. The MAC is needed to drive the
 *  switch flow; the live `EarbudsSnapshot` carries only name/battery. */
export interface SavedEarbuds {
  address: string;
  name: string;
}

/** Read the saved earbuds entry directly off disk. Returns null
 *  when the user hasn't picked one yet. Fast (single fs read);
 *  no BlueZ discovery involved. */
export async function getSavedEarbuds(): Promise<SavedEarbuds | null> {
  return await invoke<SavedEarbuds | null>("get_saved_earbuds");
}

// Event helpers — register before the worker emits, return Unlisten functions.
export function onIdentity(cb: (id: IdentityInfo) => void): Promise<UnlistenFn> {
  return listen<IdentityInfo>("vortex:identity", e => cb(e.payload));
}

export function onPeers(cb: (peers: TrustedPeer[]) => void): Promise<UnlistenFn> {
  return listen<TrustedPeer[]>("vortex:peers", e => cb(e.payload));
}

export function onScanResult(cb: (hit: ScanHit) => void): Promise<UnlistenFn> {
  return listen<ScanHit>("vortex:scan_result", e => cb(e.payload));
}

export function onScanDone(cb: () => void): Promise<UnlistenFn> {
  return listen<null>("vortex:scan_done", () => cb());
}

export function onSwitchScanning(cb: (scanning: boolean) => void): Promise<UnlistenFn> {
  return listen<boolean>("vortex:switch_scanning", e => cb(e.payload));
}

export function onSwitchCandidates(
  cb: (candidates: SwitchCandidate[]) => void,
): Promise<UnlistenFn> {
  return listen<SwitchCandidate[]>("vortex:switch_candidates", e => cb(e.payload));
}

export function onPairingStarted(cb: (e: PairingStartedEvent) => void): Promise<UnlistenFn> {
  return listen<PairingStartedEvent>("vortex:pairing_started", e => cb(e.payload));
}

export function onPairingResult(cb: (e: PairingResultEvent) => void): Promise<UnlistenFn> {
  return listen<PairingResultEvent>("vortex:pairing_result", e => cb(e.payload));
}

/**
 * SAS verification code surfaced from the daemon. Linux auto-approves
 * (UX choice), but we still display the SAS so the user can compare
 * with what the phone shows and abort by killing the app if needed.
 */
export function onPairingSas(cb: (sas: string) => void): Promise<UnlistenFn> {
  return listen<string>("vortex:pairing_sas", e => cb(e.payload));
}

/**
 * Emitted when the daemon could not read the peer store (e.g. Secret
 * Service is locked or its socket is unavailable). The UI should
 * surface a "Trust store locked" message instead of treating absence
 * of `vortex:peers` as "no trust" — otherwise the user re-pairs and
 * silently creates a duplicate trust entry once the store unlocks.
 */
export function onPeerStoreError(cb: (msg: string) => void): Promise<UnlistenFn> {
  return listen<string>("vortex:peer_store_error", e => cb(e.payload));
}

/**
 * Emitted when the daemon hit an unrecoverable startup error (most
 * commonly Secret Service not available). The UI should display a
 * blocking banner — Vortex cannot proceed without secure storage
 * because the long-term identity / trust records would otherwise
 * vanish at restart.
 */
export function onFatal(cb: (msg: string) => void): Promise<UnlistenFn> {
  return listen<string>("vortex:fatal", e => cb(e.payload));
}

/**
 * Latest local-earbuds snapshot from BlueZ on this laptop. `null` when
 * nothing is currently routed to the system. Fires on mount and after
 * each `refreshLocalEarbuds()` request.
 */
export interface EarbudsSnapshot {
  name: string;
  battery: number | null;
  connected: boolean;
}
export function onLocalEarbuds(
  cb: (e: EarbudsSnapshot | null) => void,
): Promise<UnlistenFn> {
  return listen<EarbudsSnapshot | null>("vortex:local_earbuds", e => cb(e.payload));
}

/**
 * Subscribe to peer AppState updates (battery, locale, theme, earbuds).
 * Fires after every successful reconnect — the UI can cache the value
 * and render it even when the session is idle.
 */
export function onPeerState(cb: (state: PeerState) => void): Promise<UnlistenFn> {
  return listen<PeerState>("vortex:peer_state", e => cb(e.payload));
}

/** Pull the latest per-peer state over the invoke channel — a self-heal
 *  backstop for the pushed `vortex:peer_state` events. If those silently stop
 *  arriving (a dead listener used to freeze the card at "offline"), polling
 *  this keeps the card fresh; it rides the invoke-response channel, which is
 *  independent of the event channel. */
export async function getPeerStates(): Promise<PeerState[]> {
  return await invoke<PeerState[]>("get_peer_states");
}

/** Backend lifecycle messages for the screen-mirror window ("mirror window
 *  opening", "…closed by user", "gst EOS", "gst error…"). Lets the UI track
 *  whether a mirror is live so the Share button can toggle to "Stop sharing". */
export function onMirrorPlayer(cb: (message: string) => void): Promise<UnlistenFn> {
  return listen<{ message: string }>("mirror-player", e => cb(e.payload?.message ?? ""));
}

/** Stop the live screen mirror (closes the native window + decoder + injector).
 *  A reliable, scaling-independent stop — the window's own X is unreliable under
 *  fractional display scaling. */
export async function stopScreenMirror(): Promise<void> {
  await invoke("stop_screen_mirror");
}

export function onBusy(cb: (busy: boolean) => void): Promise<UnlistenFn> {
  return listen<boolean>("vortex:busy", e => cb(e.payload));
}

// ---- Earbuds switch (Phase 1) ----

/**
 * The orchestrator's view of the in-flight earbuds switch. Mirrors
 * `audio_orchestrator::SwitchState` on the daemon side. Cards subscribe
 * to this so they can render a "Switching…" caption + dim the card while
 * a flow is in progress and surface the failure reason for ~3 s when
 * the daemon flips to Failed before auto-resetting to Idle.
 */
export type SwitchState =
  | { kind: "idle" }
  | { kind: "preparing" }
  | { kind: "waiting_approval" }
  | { kind: "waiting_released" }
  | { kind: "connecting" }
  | { kind: "almost_done" }
  | { kind: "failed"; reason: string };

/** Subscribe to switch-state changes. Fires on every CAS transition. */
export function onSwitchState(cb: (s: SwitchState) => void): Promise<UnlistenFn> {
  return listen<SwitchState>("vortex:switch_state", e => cb(e.payload));
}

/**
 * Ask the daemon to start an earbuds-switch flow as the initiator
 * (we want the buds here). The daemon opens a LAN session and runs
 * the standard request → response cycle.
 */
export async function requestEarbudsSwitch(peerStaticPub: string, mac: string): Promise<void> {
  await invoke("request_earbuds_switch", { peerStaticPub, mac });
}

/**
 * Tell the peer to claim the buds. We hold them right now; the peer
 * runs its own initiator flow on receipt. Used by the "swap" icon
 * when the buds are on the local side.
 */
export async function sendEarbudsClaim(peerStaticPub: string, mac: string): Promise<void> {
  await invoke("send_earbuds_claim", { peerStaticPub, mac });
}
