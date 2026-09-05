import { ref, computed, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { Battery, BatteryLow, BatteryMedium, BatteryFull, BatteryCharging } from "lucide-vue-next";
import {
  startScan, startPair, forgetPeer, refreshState, refreshLocalEarbuds,
  requestEarbudsSwitch, sendEarbudsClaim, getSavedEarbuds, onSwitchState,
  type SwitchState, scanBluetoothDevices, saveEarbuds, clearEarbuds,
  onScanResult, onScanDone, onPairingStarted, onPairingResult, onPairingSas,
  pairDecision, onLocalEarbuds, onBusy,
  type ScanHit, type TrustedPeer, type PairingResultEvent, type PeerState,
  type EarbudsSnapshot, type BluetoothDeviceRow,
} from "@/lib/bridge";
import { initSmartSwitch } from "@/lib/smartSwitch";
import { initNotifMirror } from "@/lib/notifMirror";
import { peers, peersLoaded, peerStates } from "@/lib/connectionStore";

// Home-page state + logic as an app-lifetime singleton (matches
// connectionStore). Pages/sub-components import these refs/fns directly (no
// prop-drilling) and the state survives route changes. initHome() runs once
// from main.ts.

export async function openBluetoothSettings() {
  // Try a few common Linux BT settings entry points; first one that
  // launches wins. Fall back to a debug log if none are present.
  const candidates = [
    ["gnome-control-center", "bluetooth"],
    ["blueberry"],
    ["blueman-manager"],
    ["systemsettings5", "bluetooth"],
  ];
  for (const argv of candidates) {
    try {
      // Tauri shell plugin would do this nicely, but we don't have it
      // wired yet — best-effort: call the future "open_bluetooth_settings"
      // Tauri command if it exists.
      void argv;
      await invoke("open_bluetooth_settings");
      return;
    } catch (_) {
      // command not registered — fall through to the next
    }
  }
}

export const scanHits = ref<ScanHit[]>([]);
export const scanning = ref(false);
export const pairingPeer = ref<string | null>(null);
export const pairingResult = ref<PairingResultEvent | null>(null);
export const pairingSas = ref<string | null>(null);
// True when the user tapped "They don't match" on the emoji security check —
// drives the aborted screen even before the backend result lands.
export const pairLocalRejected = ref(false);


/** Earbuds currently plugged into THIS laptop, if any. */
export const localEarbuds = ref<EarbudsSnapshot | null>(null);

export const forgetTarget = ref<TrustedPeer | null>(null);

/** Pair-phone modal (lists scan hits, click → pair). */
export const showPairPhoneModal = ref(false);

/** Earbuds long-press context menu (Remove from Vortex). */
export const earbudsMenuOpen = ref(false);
/** Earbuds "+ Add" modal — BT picker. */
export const earbudsAddOpen = ref(false);
export const earbudsScanning = ref(false);
export const earbudsScanResults = ref<BluetoothDeviceRow[]>([]);

/** Latest switch-state snapshot from the daemon. Seeded to `idle` so
 *  the card renders normally before the first event arrives. */
export const switchState = ref<SwitchState>({ kind: "idle" });
/** Visually "still switching" — used to dim the card and gate clicks.
 *  Note `almost_done` is excluded: at that point the peer has
 *  confirmed the release, the user sees a fully-finished card, and
 *  the local BT connect is just settling in the background. */
export const isSwitching = computed(() => {
  const k = switchState.value.kind;
  return k !== "idle" && k !== "failed" && k !== "almost_done";
});
/** Whether a flow is in any way in progress (any non-Idle/Failed state).
 *  Used to keep the swap button itself disabled — even during the
 *  optimistic-done window we don't want a second click starting a
 *  new flow while BlueZ is still settling. */
export const flowInProgress = computed(() => {
  const k = switchState.value.kind;
  return k !== "idle" && k !== "failed";
});
export async function openEarbudsPicker() {
  earbudsAddOpen.value = true;
  earbudsScanResults.value = [];
  earbudsScanning.value = true;
  try {
    earbudsScanResults.value = await scanBluetoothDevices();
  } catch (e) {
    console.warn("BT scan failed", e);
  } finally {
    earbudsScanning.value = false;
  }
}

export async function pickEarbud(d: BluetoothDeviceRow) {
  await saveEarbuds(d.address, d.name);
  earbudsAddOpen.value = false;
  // Push a fresh local-earbuds event so the card switches over
  // immediately — the worker reads the new saved entry from disk.
  refreshLocalEarbuds().catch(() => {});
}

export async function removeEarbuds() {
  await clearEarbuds();
  earbudsMenuOpen.value = false;
  refreshLocalEarbuds().catch(() => {});
}

/** Whether the UI may offer a switch action. Match the Android side:
 *  needs a paired peer and a saved earbuds row, but DOES NOT require
 *  the peer to be online right now — the flow still works on the
 *  next heartbeat once the peer comes back, and hiding the icon
 *  leaves the user with no obvious way to retry. */
export const canSwitch = computed(() =>
  primaryPeer.value != null && localEarbuds.value?.name != null
);

/** Find the buds MAC for the switch flow. Reads the saved row off
 *  disk directly — fast (single fs read), works whether or not the
 *  buds are currently bonded/connected on this laptop. The previous
 *  implementation did a 4-second BlueZ discovery, which often failed
 *  to surface buds that were connected to the phone at the time. */
export async function macForSwitch(): Promise<string | null> {
  try {
    const saved = await getSavedEarbuds();
    return saved?.address ?? null;
  } catch (e) {
    console.warn("macForSwitch: getSavedEarbuds failed", e);
    return null;
  }
}

export async function startSwitch() {
  const peer = primaryPeer.value;
  if (!peer) return;
  const mac = await macForSwitch();
  if (!mac) {
    console.warn("startSwitch: no MAC available for switch");
    return;
  }
  const direction = activeEarbuds.value?.on === "local" ? "send" : "claim";
  try {
    if (direction === "send") {
      // Buds are on local side; we send Claim so the phone becomes
      // initiator and grabs them. The orchestrator on the phone
      // turns Claim into request() — the phone now drives the
      // standard flow against us as responder.
      await sendEarbudsClaim(peer.peer_static_pub, mac);
    } else {
      await requestEarbudsSwitch(peer.peer_static_pub, mac);
    }
  } catch (e) {
    console.warn("startSwitch failed", e);
  }
}

// True briefly after the share-screen button is tapped: the laptop opens the
// mirror session, the phone raises its consent dialog, and the native window
// appears asynchronously once "Start now" is tapped. The flag just debounces
// the button + shows a spinner so it can't be spammed.
export const mirrorStarting = ref(false);
// True while a screen mirror is live, so the Share button can flip to "Stop
// sharing" — a reliable stop that doesn't depend on the native window's X
// button (whose hit-target is misplaced under fractional display scaling).
export const mirrorActive = ref(false);

// Track the mirror's lifecycle from the backend's "mirror-player" events so
// `mirrorActive` reflects reality (window opened / closed / errored), registered
// once.
let mirrorListenerSet = false;
async function ensureMirrorListener() {
  if (mirrorListenerSet) return;
  mirrorListenerSet = true;
  const { onMirrorPlayer } = await import("@/lib/bridge");
  await onMirrorPlayer((msg) => {
    // "opening" is the ONE message that means live; everything else the
    // backend sends is an ending of some kind. Matching stop-words instead
    // left the button stuck green whenever a message did not happen to
    // contain one — "mirror stopped" (what closing the window's X emits) and
    // "phone not reachable on LAN" both slipped through.
    mirrorActive.value = msg.includes("opening");
  });
}

export async function stopMirror() {
  mirrorActive.value = false;
  try {
    const { stopScreenMirror } = await import("@/lib/bridge");
    await stopScreenMirror();
  } catch (e) {
    console.warn("stopMirror failed", e);
  }
}

export async function startMirror() {
  if (!primaryPeer.value || !phoneOnline.value || mirrorStarting.value) return;
  void ensureMirrorListener();
  mirrorStarting.value = true;
  const cfg = { width: 720, height: 1560, fps: 60, bitrate: 10_000_000, transport: "wifi" };
  try {
    // Ask the phone for its best and let the laptop deal with what arrives.
    // A fixed request is wrong in both directions: measured on this device
    // (Redmi 9, Helio G80) scrcpy itself only sustains 29-40 fps at a QUARTER of
    // these pixels, so 60 is not what comes back here — but pinning the request
    // to 30 would also cap a phone that could do more. The evenness that used to
    // be missing is now the receiver's job: it measures the real delivery rate
    // and paces the picture onto a matching grid (see `mirror::pace_grid_for`).
    // Bitrate is the CEILING: the phone runs adaptive bitrate underneath it, so
    // quality dips on a weak link instead of the stream freezing.
    await invoke("start_screen_mirror", cfg);
  } catch (e) {
    console.warn("startMirror failed", e);
  } finally {
    // The stream opens async (after on-phone consent); clear the busy flag after
    // a short window so the button is usable again regardless of outcome.
    setTimeout(() => {
      mirrorStarting.value = false;
    }, 2500);
  }
}

export function onCardTap() {
  // Bidirectional: tap fires a switch regardless of which side
  // currently holds the buds. startSwitch() inspects activeEarbuds.on
  // and dispatches to the right path (claim vs. let peer claim).
  if (canSwitch.value && !flowInProgress.value) {
    startSwitch();
  }
}

export function sendToPeer() {
  earbudsMenuOpen.value = false;
  startSwitch();
}
export let earbudsPressTimer: ReturnType<typeof setTimeout> | null = null;
export function onEarbudsPressStart() {
  if (earbudsPressTimer) clearTimeout(earbudsPressTimer);
  earbudsPressTimer = setTimeout(() => {
    earbudsMenuOpen.value = true;
    earbudsPressTimer = null;
  }, 650);
}
export function onEarbudsPressEnd() {
  if (earbudsPressTimer) {
    clearTimeout(earbudsPressTimer);
    earbudsPressTimer = null;
  }
}

export const unlisten: UnlistenFn[] = [];
export let scanLoopActive = false;

export const primaryPeer = computed(() => peers.value[0] ?? null);
export const primaryPeerState = computed<PeerState | null>(() => {
  const p = primaryPeer.value;
  return p ? peerStates.value[p.peer_static_pub] ?? null : null;
});
export const peerEarbuds = computed(() => primaryPeerState.value?.earbuds ?? null);

/**
 * Unified earbuds shown on the home screen. Resolution mirrors the
 * Android side: once the user picks a device, the card stays visible
 * forever — connected here, connected on the phone, OR not connected
 * at all. The "+ Add" placeholder only appears when no row is saved.
 *
 *   • Saved row + local connected → "Connected here" + battery
 *   • Saved row + peer connected (matching name) → "Connected to phone" + peer battery
 *   • Saved row + nothing connected → "Not connected", no battery
 *   • No saved row → null (renders the "+ Add" placeholder)
 *
 * The daemon's [`scan_local_earbuds`] always emits the saved row when
 * one exists (with `connected=false` when offline), so `localEarbuds`
 * is non-null exactly when a save is present. We use that as the
 * "has-save" signal without an extra round-trip.
 */
export const activeEarbuds = computed<
  { name: string; battery: number | null; on: "local" | "peer"; connected: boolean } | null
>(() => {
  const local = localEarbuds.value;
  if (!local) return null;  // no saved row → placeholder
  // Trust the peer's report only when the phone is online — a stale
  // snapshot from a peer that's been offline for hours must not pin
  // the card at "connected" forever.
  const peerHas =
    phoneOnline.value &&
    peerEarbuds.value?.connected === true &&
    peerEarbuds.value.name.toLowerCase() === local.name.toLowerCase();
  if (local.connected) {
    return { name: local.name, battery: local.battery, on: "local", connected: true };
  }
  if (peerHas) {
    return { name: local.name, battery: peerEarbuds.value!.battery, on: "peer", connected: true };
  }
  // Saved row exists but the buds are nowhere — render the card so
  // the user can still see them and remove via long-press.
  return { name: local.name, battery: null, on: "local", connected: false };
});

// Coarse reactive clock so the freshness checks below re-evaluate as time
// passes (a bare Date.now() in a computed never re-runs on its own, so a card
// could stay "online" forever after frames stop — the user then had to reload).
// One app-lifetime ticker; 10 s is plenty for the window below.

/**
 * How recently we must have heard from the phone to call it online.
 *
 * Was 180 s here while the phone's UI used 30, its notification 30, and the
 * proximity watcher 25 — four answers to one question, and they disagreed
 * visibly: the laptop would auto-lock at ~27 s because the phone had gone,
 * while this window still said "Online" for another two and a half minutes.
 * 35 s matches the threshold the mirror pills already use for "we are in
 * contact", and is comfortably above the 12 s heartbeat.
 */
const PEER_ONLINE_SECS = 35;
const nowTick = ref(Math.floor(Date.now() / 1000));
setInterval(() => (nowTick.value = Math.floor(Date.now() / 1000)), 10_000);

/** True if we have a paired peer AND have seen its state within ~3 min. */
export const phoneOnline = computed(() => {
  const s = primaryPeerState.value;
  if (!s) return false;
  return nowTick.value - s.ts < PEER_ONLINE_SECS;
});

// Wall-clock (epoch seconds) of the last successful pair. After pairing, the
// trusted-peer card appears immediately (onPeers) but the FIRST AppState
// heartbeat only lands once the steady session reconnects — a few-second gap
// where phoneOnline is still false. Showing a blunt "Offline" there reads as a
// failure; instead we show a transient "Connecting…" for a bounded window after
// the pair, then fall back to the honest "Offline" if the phone never checks in.
export const justPairedAt = ref<number | null>(null);

/** Bridge state right after pairing: paired, not yet seen a heartbeat, but
 *  within ~30 s of a successful pair. Flips to phoneOnline the instant the
 *  first heartbeat arrives (primaryPeerState is reactive), or to phoneOffline
 *  once the window lapses. */
export const phoneConnecting = computed(() => {
  if (phoneOnline.value || !primaryPeer.value) return false;
  const t0 = justPairedAt.value;
  return t0 != null && nowTick.value - t0 < 30;
});

export async function pairWith(addr: string) {
  showPairPhoneModal.value = false;
  pairingPeer.value = addr;
  pairingResult.value = null;
  pairingSas.value = null;
  pairLocalRejected.value = false;
  await startPair(addr);
}

export function dismissPairing() {
  pairingPeer.value = null;
  pairingResult.value = null;
  pairingSas.value = null;
  pairLocalRejected.value = false;
}

// Approve / reject the current pairing session. Mark `pairingSas`
// null *before* awaiting so the buttons immediately hide and a
// double-click can't fire two decisions even though the backend
// already guards against that.
export async function approvePairing() {
  pairingSas.value = null;
  await pairDecision(true);
}

export async function rejectPairing() {
  // "They don't match" — a possible MITM. Show the aborted screen at once and
  // tell the backend to drop the session.
  pairingSas.value = null;
  pairLocalRejected.value = true;
  await pairDecision(false);
}

export async function confirmForget() {
  if (!forgetTarget.value) return;
  const pub = forgetTarget.value.peer_static_pub;
  forgetTarget.value = null;
  await forgetPeer(pub);
}

export function openPairPhoneModal() {
  showPairPhoneModal.value = true;
  if (!scanLoopActive) runScanLoop();
}

export function batteryIcon(pct: number | null, charging = false) {
  if (charging) return BatteryCharging;
  if (pct == null) return Battery;
  if (pct >= 80) return BatteryFull;
  if (pct >= 40) return BatteryMedium;
  return BatteryLow;
}

export function batteryClass(pct: number | null, charging = false) {
  // Charging wins over the low-battery red: plugged in is not a problem.
  if (charging) return "text-blue-400";
  if (pct == null) return "text-muted-foreground";
  if (pct <= 15) return "text-red-500";
  if (pct <= 30) return "text-amber-500";
  return "text-emerald-500";
}

export let pressTimer: ReturnType<typeof setTimeout> | null = null;
export function onCardPressStart(peer: TrustedPeer) {
  if (pressTimer) clearTimeout(pressTimer);
  pressTimer = setTimeout(() => {
    forgetTarget.value = peer;
    pressTimer = null;
  }, 650);
}
export function onCardPressEnd() {
  if (pressTimer) {
    clearTimeout(pressTimer);
    pressTimer = null;
  }
}

// ---- Continuous BLE scan while no trust or pair-modal open ----
export async function runScanLoop() {
  if (scanLoopActive) return;
  scanLoopActive = true;
  while (
    scanLoopActive &&
    peersLoaded.value &&
    (peers.value.length === 0 || showPairPhoneModal.value) &&
    true && // home is only mounted on the '/' route, so always active here
    pairingPeer.value === null
  ) {
    scanHits.value = [];
    scanning.value = true;
    await startScan();
    await new Promise<void>(resolve => {
      const deadline = setTimeout(resolve, 11000);
      const tick = setInterval(() => {
        if (!scanning.value) {
          clearTimeout(deadline);
          clearInterval(tick);
          resolve();
        }
      }, 200);
    });
    if (!scanLoopActive) break;
    if (!showPairPhoneModal.value && peers.value.length > 0) break;
    await new Promise(r => setTimeout(r, 3000));
  }
  scanLoopActive = false;
}

export function maybeStartScanLoop() {
  if (
    peersLoaded.value &&
    (peers.value.length === 0 || showPairPhoneModal.value) &&
    true && // home is only mounted on the '/' route, so always active here
    pairingPeer.value === null
  ) {
    runScanLoop();
  }
}

watch([peers, peersLoaded, pairingPeer, showPairPhoneModal], () => maybeStartScanLoop());

// ---- Proximity pairing (AirPods-style "bring the phone near → sheet") ----
// The scan loop above already runs continuously while NO phone is paired, so
// scanHits is live. When an unpaired Vortex phone shows up VERY close (strong
// RSSI = a deliberate bring-near, not just in the building), pop the proximity
// sheet for it. Per-device dismiss + the close threshold keep it from nagging.
export const proximityHit = ref<ScanHit | null>(null);
const proximityDismissed = new Set<string>();
const PROX_RSSI = -55; // ~within arm's reach; weaker hits never auto-prompt

watch(
  scanHits,
  (hits) => {
    if (peers.value.length > 0) return; // already paired — no auto-prompt
    if (pairingPeer.value || showPairPhoneModal.value || proximityHit.value) return;
    const near = hits.find(
      (h) => (h.rssi ?? -100) > PROX_RSSI && !proximityDismissed.has(h.addr),
    );
    if (near) proximityHit.value = near;
  },
  { deep: true },
);
// Clear the sheet if a pairing starts or a phone gets paired out from under it.
watch([pairingPeer, peers], () => {
  if (pairingPeer.value || peers.value.length > 0) proximityHit.value = null;
});

/** "Not now" — hide the sheet and don't re-prompt for this device this session. */
export function dismissProximity() {
  if (proximityHit.value) proximityDismissed.add(proximityHit.value.addr);
  proximityHit.value = null;
}
/** "Pair" on the proximity sheet → run the normal pairing flow for that device. */
export async function pairFromProximity() {
  const addr = proximityHit.value?.addr;
  proximityHit.value = null;
  if (addr) await pairWith(addr);
}

export let _homeStarted = false;
export async function initHome() {
  if (_homeStarted) return;
  _homeStarted = true;
  // identity / peers / peer-state are subscribed once in connectionStore.
  unlisten.push(await onScanResult(hit => {
    // Two-stage dedup. Same physical adv (same BD_ADDR) emits multiple
    // times — once on ServiceData, again when the SCAN_RSP arrives and
    // BlueZ updates the Name property; we want one row that gains the
    // name on the second pass. Then a single phone that restarts
    // advertise rotates its random BD_ADDR but keeps the same BT alias
    // — those two BD_ADDRs collapse on `name`, and we always keep the
    // *newest* observation: stale BlueZ cache entries hold their last
    // RSSI long after the phone has rotated, so a "stronger RSSI wins"
    // rule would lock the row onto a dead address and the user's tap
    // would hang for ~20 s on a connect attempt that never reaches the
    // live peripheral.
    const byAddr = scanHits.value.findIndex(h => h.addr === hit.addr);
    if (byAddr !== -1) {
      const cur = scanHits.value[byAddr];
      scanHits.value[byAddr] = {
        ...cur,
        rssi: hit.rssi,
        name: hit.name ?? cur.name,
        instance: hit.instance,
      };
      return;
    }
    if (hit.name) {
      const byName = scanHits.value.findIndex(h => h.name === hit.name);
      if (byName !== -1) {
        scanHits.value[byName] = hit;
        return;
      }
    }
    scanHits.value.push(hit);
  }));
  unlisten.push(await onScanDone(() => (scanning.value = false)));
  unlisten.push(await onPairingStarted(e => {
    pairingPeer.value = e.peer_addr;
    pairingResult.value = null;
  }));
  unlisten.push(await onPairingResult(r => {
    pairingResult.value = r;
    // Open the "Connecting…" grace window so the freshly-paired card doesn't
    // flash "Offline" before the first heartbeat reconnects.
    if (r.ok) justPairedAt.value = Math.floor(Date.now() / 1000);
  }));
  unlisten.push(await onPairingSas(sas => {
    // Show the emoji security check (read-only on the laptop) and auto-approve
    // from this side — the laptop user already picked this phone, so that tap is
    // their intent. The PHONE is the sole decision point: the user compares the
    // 3 emoji there and taps Approve / Decline. A mismatch → they decline on the
    // phone → the pairing fails here.
    pairingSas.value = sas;
    pairDecision(true).catch(() => {});
  }));
  unlisten.push(await onLocalEarbuds(snap => {
    localEarbuds.value = snap;
  }));
  unlisten.push(await onSwitchState(s => {
    const prev = switchState.value.kind;
    switchState.value = s;
    // Force a state refresh the moment a flow transitions out of an
    // active phase. BlueZ takes a beat to reflect the new connection
    // state; without the explicit refresh we wait the 5 s poll tick
    // and the UI shows stale "Switching…" / "Not connected" briefly.
    const wasActive = prev !== "idle" && prev !== "failed";
    const nowDone = s.kind === "idle" || s.kind === "failed";
    if (wasActive && nowDone) {
      // Quick-fire a few refreshes — BlueZ + audio routing settles
      // over ~1 s. Spaced out to catch the moment A2DP marks the
      // device connected without burning CPU.
      [50, 400, 1000].forEach(ms =>
        setTimeout(() => refreshLocalEarbuds().catch(() => {}), ms),
      );
    }
  }));
  // Initial probe; the steady 5 s rescan lives in the Rust backend now
  // (lib.rs heartbeat) — a webview interval here gets throttled by WebKit
  // whenever the window is hidden, which froze the tray rows.
  refreshLocalEarbuds().catch(() => {});
  unlisten.push(await onBusy(_b => { /* reserved */ }));
  // Push the initial locale state to the daemon so the very first
  // outgoing AppState carries it. If we're on the system default
  // (changedAt == 0) we still push so the worker overrides its
  // own system_locale() guess with the one the UI actually shows.
  await refreshState();
  // Seed the smart-switch toggle from the daemon (its persisted value) and
  // subscribe to peer-driven LWW changes so the UI tracks them live.
  initSmartSwitch();
  initNotifMirror();
  maybeStartScanLoop();
}

