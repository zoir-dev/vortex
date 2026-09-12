# Multi-peer pairing: one phone ↔ many laptops, one laptop ↔ many phones

**Status:** design, not implemented. **Scope:** V1.x, no wire break.

Goal: a phone may remember several laptops and a laptop several phones, with
**at most one active link at a time**. Switching between remembered devices must
not require forgetting one of them.

The "not simultaneously" constraint is what keeps this small. It removes session
multiplexing, concurrent transports, and conflict resolution on shared state.
What remains is: be *discoverable* by all remembered peers, *pick* one, and
*scope* per-peer state.

---

## 1. What already works

Multi-peer was partly anticipated. Verified in the current tree:

| Capability | Where | State |
|---|---|---|
| Laptop accepts *any* trusted peer's presence token | `linux/ui-tauri/src-tauri/src/ble.rs:100` (`expected_presence_tokens`) | iterates all peers ✅ |
| Phone's IK responder identifies *which* peer is calling | `ReconnectOrchestrator.kt:132` — tries each peer's PRS as prologue | multi-peer ✅ |
| Per-peer secrets (PRS, counters, nonces, bonded addr) | Secret Service attrs keyed by `peer_static_pub` | already per-peer ✅ |
| Laptop can pair-scan while trust exists | `useHome.ts` `runScanLoop` condition includes `showPairPhoneModal` | ✅ |
| Protocol is additively extensible | `shared/proto/vortex.proto` — *"New payload types must go through `VortexMessage.payload` oneof only"*, plus `capability_flags` | ✅ |

So this is not a rewrite.

## 2. The blocker

Discovery identity is welded to exactly one peer:

```
token = HMAC-SHA256(peer.prs, "vortex/v1/presence" ‖ u64_be(bucket))[0..8]
```

There is one 8-byte slot, and the ADV_IND is already at the legacy 31-byte
ceiling (3-byte Flags AD + 28-byte Service Data AD) — no room for a second
token. The *same* construction names the mDNS record
(`LanServer.kt:derivePrivateInstanceName`).

Both call sites resolve the peer with `peerStore.list().firstOrNull()`. So "who
can find me" is a per-PRS property fixed to peer[0], and forgetting is the only
way to change it. Everything else below is downstream of this one fact.

---

## 3. Decisions

### D1 — Keep per-peer tokens; time-multiplex one advertising set

While seeking, the phone cycles candidate peers on a single advertising set,
dwelling ~1–2 s per token. Worst-case discovery is `(N−1) × dwell` — a few
seconds, on a deliberate, user-initiated handoff.

**Rejected: one shared device-level presence key** (`PRS_pres` distributed at
pairing, so a single token serves every peer). It is cheaper on air, but:

- it is a wire-contract change, and
- a *revoked* laptop retains the ability to recognise the phone until the key is
  rotated and redistributed to every survivor.

Per-peer tokens have no such revocation hole: forgetting a peer deletes its PRS,
so the phone can no longer derive that token at all. A forgotten laptop becomes
exactly as able to find the phone as a stranger's — not at all. **Per-peer tokens
are both cheaper to ship and strictly better for revocation.**

**Rejected: N concurrent advertising sets.** Viable on hardware (the test phone
reports `max_adv_instances: 16`), but costs N× radio duty cycle permanently, and
OEM background-advertising throttling — already documented in `Advertiser.kt` —
makes it degrade unevenly across phones. Unnecessary once seeking is rare.

### D2 — No wire break, and **no new advertising flag bit**

`AdvFlags::is_well_formed()` (Rust `core/ble/mod.rs`, mirrored in
`AdvPayload.kt:29`) requires reserved bits zero **and exactly one** of
bit0 `PAIRABLE` / bit1 `TRUSTED_PRESENCE`. A third "SEEKING" mode bit would be
rejected as malformed by **every deployed peer**.

Seeking therefore advertises `flags = TRUSTED_PRESENCE` and varies only
`AdvertiseSettings` mode and dwell. The state is local; the wire is unchanged.

### D3 — "Switch" seeks *before* it releases

Switch is **not** a release. The current link is held while the device scans (or
advertises) for another *already-remembered* peer. Only once a replacement is
identified — and chosen, if there is more than one — is the old link dropped.

This is why there is no standby/suppression window: the device never lets go, so
its own reconnect loop has nothing to race back into. The current peer is
excluded from the candidate set by construction. Notably it also cannot get
stuck connected to nothing.

Switch is **not** Forget. Forget already exists and remains the escape hatch for
a peer that is out of reach (see §7).

### D4 — Separate *connected* from *active*

The arbiter owns exactly one `activePeer`. Confirming a switch flips ownership
**atomically**, demoting the old peer with an explicit "no longer active" reason.

Transport teardown may then be lazy. Without this split, "connect to B and let
backoff drop A" leaves two live links that both mirror notifications and sync
clipboard — duplicated state, and a violation of the one-active-link rule. With
it, overlapping *connections* are harmless because only one is *active*.

Losers of an arbitration race must receive an explicit **busy** refusal so they
back off rather than hammering the phone's single GATT link.

### D5 — Phone advertising state machine

| State | Advertising | Enter on |
|---|---|---|
| `Pairable` | LOW_LATENCY, `PAIRABLE` flag | peer list **empty**, or explicit "Add pair" |
| `Active` | **none** | `activePeer` connected |
| `Seeking` | `TRUSTED_PRESENCE`, backoff ladder, tokens multiplexed (D1) | Switch pressed, or link down |
| `Dark` | **today's steady-state advertising** (see below) | seeking ladder exhausted |

`Active` = silent is the main battery win: the phone is connected most of the
time, and today it advertises 24/7 regardless.

Ladder: LOW_LATENCY ≈ 30 s (the user is walking to the other machine) →
BALANCED a few minutes → LOW_POWER → `Dark`.

**`Dark` keeps advertising** rather than going silent. Two hard reasons:

1. **Proximity lock depends on it.** `proximity.rs` treats "not away" as *active
   session **or** token-validated advertisement*, with `AWAY_GRACE_MS = 25_000`
   and a `CONFIRM_SCAN_MS = 2_000` last-chance scan whose sizing comment
   explicitly assumes *"a present phone is in the reconnect-seeking LOW_LATENCY
   tier (~100 ms adv) when this runs."* A silent-but-present phone would be
   locked out spuriously. The ladder floor must stay detectable inside a 2 s
   scan.
2. **It preserves the walk-back-to-desk auto-reconnect** promised in the README
   ("devices reconnect on their own"), which a silent `Dark` would regress.

`Active` = silent is nonetheless safe for proximity, because a live session is
itself the presence proof.

### D6 — Exiting `Dark`

Trigger: **screen on + Vortex app brought to foreground/focus.** Wanting to
reconnect is a deliberate act; requiring the app in front is acceptable UX.

**Rejected: exit `Dark` on joining a Wi-Fi network where a peer was last seen.**
It is nearly free (mDNS, no BLE) and was tempting, but Wi-Fi coverage is much
larger than BLE and can be spotty. A walk-away could drop BLE, drop Wi-Fi, then
*re-acquire* Wi-Fi while still far from the laptop, reconnecting and letting the
laptop unlock from well outside BLE range. That defeats proximity lock/unlock.
LAN must not be used as a proximity signal.

Since `Dark` still advertises (D5), the cost of dropping this is small.

### D7 — Namespace per-peer state by public key, not name

```
~/.cache/vortex/peers/<hex(peer_static_pub)[0..16]>/{sms,contacts,call_log}.json
```

Display name lives *inside* the folder as data. The peer name arrives from the
peer's APPROVE payload and is already sanitised on both sides
(`sanitize_peer_name`) precisely because it is untrusted — it must never become
a path component. Names also collide ("Laptop"), change, and may be non-ASCII.

Splits per peer: SMS, contacts, call log, notifications, media state.
Stays global (deliberately one shared list): notes/todos, clipboard history.

### D8 — UI

**Laptop (Tauri).** The connected-peer card gains a header "Switch device"
icon-button. Disabled when no peer is connected — with nothing connected the
device is already seeking and will find any remembered peer on its own, so the
button has no work to do. Prefer an inline "Switching… **Cancel**" affordance
over a confirm modal: the action is cheap and reversible during the seek window.

Icon must be a bundled inline SVG, never a CDN reference — the app is
offline-first by design, and hotlinked icon sets carry attribution terms. It has
to read at 16–20 px, so keep any phone glyph *outside* the arrow arc; nested
detail mushes at that size.

**Phone.** Mirror the Switch button on the connected-laptop card, plus an "Add
pair" button (the laptop already has one) so pairable mode is reachable while
trust exists — today `MainActivity.onResume` only auto-enters pairable when
`peerStore.list().isEmpty()`.

**Picker.** When more than one candidate is found, ask which to connect to, MRU
order, with last-seen. Excludes the currently-active peer (D3). A single
candidate connects automatically. This also handles two laptops seeking the same
phone: the phone asks, rather than silently arbitrating.

### D9 — Bound the seek

Switch must have an auto-expiry and a visible Cancel. Seeking *on top of* a live
connection is the most expensive state in the system, so an unattended Switch
(press it, walk to a room with no laptop) must not scan indefinitely — it
returns to `Active`.

---

## 4. Protocol additions

Additive only, gated on the existing `capability_flags`:

- `PeerHandoff` in the `VortexMessage.payload` oneof — carries "release me / you
  are no longer active", with a reason code (`switched`, `busy`, `revoked`).

Old peers ignore an unknown oneof field; the capability bit prevents sending it
to a peer that would not act on it. No change to `AdvPayload`, the advertising
flags, the GATT UUIDs, or the token derivation.

## 5. Sites to change

**Android — `firstPeer` → `activePeer`:**

| Site | Role |
|---|---|
| `MainActivityPairing.kt:234` | advertising-mode selection — *the blocker* |
| `LanServer.kt:1039` | mDNS instance name |
| `VortexStack.kt:678, 721, 740, 752, 899` | service-stack peer binding |
| `MainActivityEarbuds.kt:77` | earbuds switch target |
| `HomeScreen.kt:107` | UI primary card → device list |

**Linux:** no first-peer assumptions in trust/session logic. Work is the cache
namespacing (D7), the arbiter (D4), and the Switch UI (D8).

## 6. Sequencing

1. **Forget drops the BT bond on both sides** + **"Add pair" on the phone**.
   No protocol work. Unblocks the immediate pain.
2. **Per-peer cache namespacing** (D7). Prerequisite for laptop ↔ N phones.
3. **Arbiter + Switch flow** (D3, D4, D8, D9) and the phone state machine (D5,
   D6).

Steps 1–2 are independent and separately shippable.

## 7. Prerequisite: Forget is currently broken

`BondCleaner.removeBond` is reachable **only** from the DEBUG dev-hook intent
(`MainActivity.kt:269`); `onForgetPeerClicked` never calls it, and
`cmd_pairing.rs` has no `remove_device`/unpair either. So Vortex's Forget leaves
the BT bond in place on *both* sides.

A one-sided bond — laptop's bond dropped, phone's retained — makes the link tear
down during encryption, so `ServicesResolved` never arrives and pairing fails
with `timeout: service discovery`. Observed live 2026-08-25; it is the cause of
"retry pairing several times until it works".

Because Switch is disabled when the peer is absent (D8), **Forget is the only
escape from a phone bound to an unreachable laptop.** That escape hatch has to
work before this design can rely on it.

## 8. Open risks

- **Multiplex dwell vs. OEM throttling.** Restarting an advertising set every
  1–2 s may land in a slower throttle tier on aggressive ROMs, and each restart
  re-randomises the RPA. The laptop matches on token, not address, so this is
  correctness-safe, but it inflates BlueZ's device cache — a known source of
  stale-RPA connect attempts. Needs measurement on a throttling ROM.
- **`Dark` re-entry latency.** Requiring app-foreground (D6) means a phone that
  ladder-expired needs a deliberate user action to become *fast* again. Mitigated
  by `Dark` still advertising, so reconnect works, just slower.
- **Arbitration during simultaneous switch.** Both sides pressing Switch at once
  is untested territory; the atomic `activePeer` flip (D4) should make it safe
  but needs an explicit test.
