package com.vortex.a3.ui.screens

import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Laptop
import androidx.compose.material.icons.outlined.Settings
import androidx.compose.material.icons.outlined.Smartphone
import androidx.compose.material.icons.outlined.StickyNote2
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight as FW
import androidx.compose.ui.unit.dp
import com.vortex.a3.R
import com.vortex.a3.core.appstate.AppState
import com.vortex.a3.core.appstate.EarbudsInfo
import com.vortex.a3.core.earbuds.BluetoothDeviceRow
import com.vortex.a3.core.earbuds.SwitchState
import com.vortex.a3.core.identity.IdentityRecord
import com.vortex.a3.core.pairing.PairingOrchestrator
import com.vortex.a3.core.pairing.ReconnectOrchestrator
import com.vortex.a3.core.storage.TrustedPeer
import com.vortex.a3.ui.AdvertiseState
import com.vortex.a3.ui.LAPTOP_STALE_MS
import com.vortex.a3.ui.PickerState
import com.vortex.a3.ui.components.CardCorner
import com.vortex.a3.ui.components.EarbudsCard
import com.vortex.a3.ui.components.EarbudsPickerDialog
import com.vortex.a3.ui.components.HintCard
import com.vortex.a3.ui.components.OtherPeersCard
import com.vortex.a3.ui.components.PeerDeviceCard
import com.vortex.a3.ui.components.SurfaceCard
import com.vortex.a3.ui.components.VortexDivider
import com.vortex.a3.ui.components.WaitingForLinuxRow
import com.vortex.a3.ui.components.toHex
import com.vortex.a3.ui.str

/**
 * The home screen — laptop peer card + earbuds card, plus the pre-trust
 * waiting state when no peer is paired. Also owns the long-press
 * "Forget device" confirm and the in-app BT picker modal.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun HomeScreen(
    state: AdvertiseState,
    @Suppress("UNUSED_PARAMETER") identity: IdentityRecord?,
    @Suppress("UNUSED_PARAMETER") handshake: PairingOrchestrator.HandshakeOutcome?,
    @Suppress("UNUSED_PARAMETER") reconnect: ReconnectOrchestrator.ReconnectOutcome?,
    peers: List<TrustedPeer>,
    peerStates: Map<String, AppState>,
    peerLastSeen: Map<String, Long>,
    now: Long,
    localEarbuds: EarbudsInfo?,
    hasSavedEarbuds: Boolean,
    pickerState: PickerState,
    switchState: SwitchState,
    onForgetPeer: (TrustedPeer) -> Unit,
    onAddPair: () -> Unit,
    onCancelAddPair: () -> Unit,
    onSwitchLaptop: () -> Unit,
    onSwitchToPeer: (TrustedPeer) -> Unit,
    seekingLaptop: Boolean,
    onOpenAutostart: () -> Unit,
    onDismissAutostartHint: () -> Unit,
    onRequestBatteryWhitelist: () -> Unit,
    onOpenSettings: () -> Unit,
    onOpenNotes: () -> Unit,
    onOpenEarbudsPicker: () -> Unit,
    onPickEarbud: (BluetoothDeviceRow) -> Unit,
    onRescanEarbuds: () -> Unit,
    onClosePicker: () -> Unit,
    onRemoveSavedEarbuds: () -> Unit,
    onToggleLaptopLock: (Boolean) -> Unit,
    showAutostartHint: Boolean,
    showBatteryHint: Boolean,
    showBluetoothOff: Boolean,
    onEnableBluetooth: () -> Unit,
) {
    val peerCount = peers.size
    // Show the laptop we are actually talking to.
    //
    // This used to be `peers.firstOrNull()`, which with two trusted laptops
    // meant "an arbitrary one" — observed showing a laptop 40 km away as
    // "Disconnected" while the phone was happily syncing with the one on the
    // desk. Freshest traffic wins; `pairedAt` breaks ties so the choice is
    // still deterministic when nothing has been heard from yet (and then it is
    // most-recently-paired, which is the best available guess at "yours").
    val primaryPeer = peers.maxWithOrNull(
        compareBy<TrustedPeer> { peerLastSeen[it.peerStaticPub.toHex()] ?: 0L }
            .thenBy { it.pairedAt },
    )
    val primaryState = primaryPeer?.let { peerStates[it.peerStaticPub.toHex()] }
    val primaryHex = primaryPeer?.peerStaticPub?.toHex()
    val lastSeen = primaryHex?.let { peerLastSeen[it] } ?: 0L
    val isLaptopOnline = primaryPeer != null && lastSeen > 0L &&
        (now - lastSeen) < LAPTOP_STALE_MS
    var forgetTarget by remember { mutableStateOf<TrustedPeer?>(null) }
    // Asking which kind of laptop screen to show — a second monitor, or a view of
    // the one that is already there.
    var showScreenKind by remember { mutableStateOf(false) }

    // Pick whichever earbuds to show. Buds physically connect to only
    // one host at a time, so we combine three sources: localEarbuds,
    // primaryState.earbuds, hasSavedEarbuds. Saved row pins the card
    // so it doesn't disappear when buds bounce between sides.
    data class ActiveEarbuds(val name: String, val battery: Int?, val onLocal: Boolean, val connected: Boolean)
    val peerBuds = primaryState?.earbuds
    val activeEarbuds: ActiveEarbuds? = when {
        hasSavedEarbuds && localEarbuds != null -> {
            val peerHas =
                isLaptopOnline && peerBuds?.connected == true &&
                    peerBuds.name.equals(localEarbuds.name, ignoreCase = true)
            when {
                localEarbuds.connected ->
                    ActiveEarbuds(localEarbuds.name, localEarbuds.battery, onLocal = true, connected = true)
                peerHas ->
                    ActiveEarbuds(localEarbuds.name, peerBuds!!.battery, onLocal = false, connected = true)
                else ->
                    ActiveEarbuds(localEarbuds.name, null, onLocal = true, connected = false)
            }
        }
        localEarbuds?.connected == true ->
            ActiveEarbuds(localEarbuds.name, localEarbuds.battery, onLocal = true, connected = true)
        isLaptopOnline && peerBuds?.connected == true ->
            ActiveEarbuds(peerBuds.name, peerBuds.battery, onLocal = false, connected = true)
        else -> null
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(MaterialTheme.colorScheme.background),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 20.dp, vertical = 16.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Image(
                painter = painterResource(R.drawable.vortex_logo),
                contentDescription = null,
                contentScale = ContentScale.Fit,
                modifier = Modifier
                    .size(36.dp)
                    .clip(RoundedCornerShape(8.dp)),
            )
            Spacer(modifier = Modifier.width(12.dp))
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    str("app.title"),
                    color = MaterialTheme.colorScheme.onSurface,
                    fontWeight = FW.SemiBold,
                    style = MaterialTheme.typography.titleMedium,
                )
                Text(
                    str("app.tagline"),
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    style = MaterialTheme.typography.bodySmall,
                )
            }
            // Notes + Settings only matter once a laptop is paired — on first
            // launch (no device in the store) the header stays bare so the
            // pairing/waiting card is the sole focus, mirroring the laptop where
            // the whole sidebar is hidden until a peer exists.
            if (peerCount > 0) {
                IconButton(onClick = onOpenNotes) {
                    Icon(
                        imageVector = Icons.Outlined.StickyNote2,
                        contentDescription = str("notes.title"),
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                IconButton(onClick = onOpenSettings) {
                    Icon(
                        imageVector = Icons.Outlined.Settings,
                        contentDescription = str("settings.title"),
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }
        VortexDivider()

        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(20.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            // Bluetooth is the floor for everything (BLE pairing + reconnect),
            // so this banner sits ABOVE all other hints and shows regardless of
            // whether a peer is paired yet — it auto-clears when BT comes back.
            if (showBluetoothOff) {
                HintCard(
                    text = str("hint.bluetooth_off"),
                    actionLabel = str("hint.bluetooth_off_action"),
                    onAction = onEnableBluetooth,
                )
            }
            if (peerCount == 0) {
                SurfaceCard {
                    Text(
                        str("discover.title"),
                        color = MaterialTheme.colorScheme.onSurface,
                        fontWeight = FW.SemiBold,
                        style = MaterialTheme.typography.titleSmall,
                    )
                    Spacer(modifier = Modifier.height(12.dp))
                    WaitingForLinuxRow(state = state)
                }
            } else {
                // Mirror the laptop's Devices page: a full-width "This device"
                // card on top, then the paired Laptop + Earbuds side by side.
                ThisPhoneCard()
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    PeerDeviceCard(
                        modifier = Modifier.weight(1f),
                        icon = Icons.Outlined.Laptop,
                        name = primaryState?.name?.takeIf { it.isNotBlank() }
                            ?: primaryPeer?.peerName?.takeIf { it.isNotBlank() }
                            ?: str("device.linux"),
                        caption = str(if (isLaptopOnline) "peers.online" else "peers.offline"),
                        // Battery/charging are only meaningful while the laptop
                        // is online — a stale AppState must not keep showing the
                        // last % or a "charging" bolt once it's disconnected.
                        battery = primaryState?.battery.takeIf { isLaptopOnline },
                        charging = isLaptopOnline && primaryState?.charging == true,
                        statusDotColor = if (isLaptopOnline) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.onSurfaceVariant,
                        onLongPress = { primaryPeer?.let { forgetTarget = it } },
                        // Remote lock: only rendered when the laptop is
                        // online AND reports its lock state.
                        locked = primaryState?.locked.takeIf { isLaptopOnline },
                        onToggleLock = {
                            primaryState?.locked?.let { onToggleLaptopLock(it) }
                        },
                        // Show the laptop's screen on this phone — only while the
                        // laptop is online (it has to be reachable to cast).
                        // Which KIND is asked here rather than assumed: the two
                        // are different enough that guessing is always wrong for
                        // half the taps.
                        onViewScreen = if (isLaptopOnline) {
                            { showScreenKind = true }
                        } else {
                            null
                        },
                        // Only offered with a second laptop to switch TO —
                        // otherwise the seek can only ever time out.
                        onSwitch = if (peerCount > 1) onSwitchLaptop else null,
                        seeking = seekingLaptop,
                    )

                    EarbudsCard(
                        modifier = Modifier.weight(1f),
                        name = activeEarbuds?.name,
                        battery = activeEarbuds?.battery,
                        connected = activeEarbuds?.connected == true,
                        onLocal = activeEarbuds?.onLocal == true,
                        canRemove = hasSavedEarbuds,
                        switchState = switchState,
                        onOpenPicker = onOpenEarbudsPicker,
                        onRemoveSaved = onRemoveSavedEarbuds,
                    )
                }

                // Every other paired laptop, tappable to switch to it.
                // Compact rows: a 180 dp card each would push the rest of the
                // screen away for what is mostly "this one exists".
                OtherPeersCard(
                    peers = peers.filter { it !== primaryPeer },
                    lastSeen = peerLastSeen,
                    now = now,
                    seeking = seekingLaptop,
                    onSwitchTo = onSwitchToPeer,
                )

                // "Pair another laptop". A pairable window preempts the
                // trusted-presence beacon (one advertising set), so it is
                // explicit and bounded rather than always-on: while it is
                // open the already-paired laptop cannot see this phone.
                val windowOpen =
                    state is AdvertiseState.Active || state is AdvertiseState.Starting
                SurfaceCard {
                    if (windowOpen) {
                        Text(
                            str("discover.title"),
                            color = MaterialTheme.colorScheme.onSurface,
                            fontWeight = FW.SemiBold,
                            style = MaterialTheme.typography.titleSmall,
                        )
                        Spacer(modifier = Modifier.height(12.dp))
                        WaitingForLinuxRow(state = state)
                        Spacer(modifier = Modifier.height(12.dp))
                        TextButton(onClick = onCancelAddPair) {
                            Text(str("peers.add_pair_cancel"))
                        }
                    } else {
                        Text(
                            str("peers.add_pair_hint"),
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            style = MaterialTheme.typography.bodySmall,
                        )
                        Spacer(modifier = Modifier.height(12.dp))
                        Button(onClick = onAddPair) {
                            Text(str("peers.add_pair"))
                        }
                    }
                }
            }

            if (showBatteryHint && peerCount > 0) {
                HintCard(
                    text = str("hint.battery"),
                    actionLabel = str("hint.battery_action"),
                    onAction = onRequestBatteryWhitelist,
                )
            }
            // MIUI Autostart standing reminder. Its dialog is one-shot, but
            // since Autostart can't be read back programmatically the card
            // can't auto-clear — so it stays until the user taps "Got it"
            // (after enabling it), which persists a dismiss flag.
            if (showAutostartHint && peerCount > 0) {
                HintCard(
                    text = str("hint.autostart"),
                    actionLabel = str("hint.autostart_action"),
                    onAction = onOpenAutostart,
                    dismissLabel = str("hint.dismiss"),
                    onDismiss = onDismissAutostartHint,
                )
            }

        }
    }

    if (pickerState.open) {
        EarbudsPickerDialog(
            pickerState = pickerState,
            onPick = onPickEarbud,
            onRescan = onRescanEarbuds,
            onClose = onClosePicker,
        )
    }

    if (showScreenKind) {
        AlertDialog(
            onDismissRequest = { showScreenKind = false },
            containerColor = MaterialTheme.colorScheme.surface,
            title = {
                Text(
                    str("cast.kind_title"),
                    color = MaterialTheme.colorScheme.onSurface,
                    fontWeight = FW.SemiBold,
                )
            },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    ScreenKindRow(
                        title = str("cast.kind_extend"),
                        hint = str("cast.kind_extend_hint"),
                    ) {
                        showScreenKind = false
                        com.vortex.a3.core.mirror.LaptopMirror.requestView(extend = true)
                    }
                    ScreenKindRow(
                        title = str("cast.kind_mirror"),
                        hint = str("cast.kind_mirror_hint"),
                    ) {
                        showScreenKind = false
                        com.vortex.a3.core.mirror.LaptopMirror.requestView(extend = false)
                    }
                }
            },
            // The choices ARE the confirmation; a second "OK" would only add a tap.
            confirmButton = {
                TextButton(onClick = { showScreenKind = false }) {
                    Text(str("switch.cancel"), color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            },
        )
    }

    if (forgetTarget != null) {
        val target = forgetTarget!!
        val displayName = peerStates[target.peerStaticPub.toHex()]?.name
            ?: target.peerName
            ?: str("device.linux")
        AlertDialog(
            onDismissRequest = { forgetTarget = null },
            containerColor = MaterialTheme.colorScheme.surface,
            title = {
                Text(str("peers.forget_title"), color = MaterialTheme.colorScheme.onSurface, fontWeight = FW.SemiBold)
            },
            text = {
                Text(
                    str("peers.forget_body", displayName),
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    style = MaterialTheme.typography.bodyMedium,
                )
            },
            confirmButton = {
                Button(
                    onClick = {
                        val t = forgetTarget
                        forgetTarget = null
                        if (t != null) onForgetPeer(t)
                    },
                    colors = ButtonDefaults.buttonColors(
                        containerColor = MaterialTheme.colorScheme.error,
                        contentColor = MaterialTheme.colorScheme.onPrimary,
                    ),
                ) { Text(str("peers.forget_confirm")) }
            },
            dismissButton = {
                TextButton(onClick = { forgetTarget = null }) {
                    Text(str("scan.close"), color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            },
        )
    }
}

/**
 * The full-width "This device" card — the phone's own tile, mirroring the
 * laptop's top card on its Devices page. Always "here", so a solid primary
 * status dot; no battery/actions (this device is the one you're holding).
 */
@Composable
private fun ThisPhoneCard(modifier: Modifier = Modifier) {
    Row(
        modifier = modifier
            .fillMaxWidth()
            .clip(CardCorner)
            .background(MaterialTheme.colorScheme.surface)
            .border(width = 1.dp, color = MaterialTheme.colorScheme.outline, shape = CardCorner)
            .padding(16.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            modifier = Modifier
                .size(42.dp)
                .clip(RoundedCornerShape(12.dp))
                .background(MaterialTheme.colorScheme.primary.copy(alpha = 0.15f)),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                imageVector = Icons.Outlined.Smartphone,
                contentDescription = null,
                tint = MaterialTheme.colorScheme.primary,
                modifier = Modifier.size(22.dp),
            )
        }
        Spacer(modifier = Modifier.width(14.dp))
        Column(modifier = Modifier.weight(1f)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    str("device.this"),
                    color = MaterialTheme.colorScheme.onSurface,
                    fontWeight = FW.SemiBold,
                    style = MaterialTheme.typography.bodyLarge,
                )
                Spacer(modifier = Modifier.width(8.dp))
                Box(
                    modifier = Modifier
                        .clip(RoundedCornerShape(50))
                        .background(MaterialTheme.colorScheme.primary.copy(alpha = 0.12f))
                        .padding(horizontal = 8.dp, vertical = 3.dp),
                ) {
                    Text(
                        str("device.this").uppercase(),
                        color = MaterialTheme.colorScheme.primary,
                        fontWeight = FW.SemiBold,
                        style = MaterialTheme.typography.labelSmall,
                    )
                }
            }
            Spacer(modifier = Modifier.height(6.dp))
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(
                    modifier = Modifier
                        .size(8.dp)
                        .clip(CircleShape)
                        .background(MaterialTheme.colorScheme.primary),
                )
                Spacer(modifier = Modifier.width(8.dp))
                Text(
                    str("device.android"),
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    style = MaterialTheme.typography.bodySmall,
                )
            }
        }
    }
}

/**
 * One choice in the "which laptop screen" dialog: a title and a line saying what
 * it actually does. Tapping it IS the answer — the dialog closes and the request
 * goes out, so there is no second confirmation to hunt for.
 */
@Composable
private fun ScreenKindRow(title: String, hint: String, onClick: () -> Unit) {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(12.dp))
            .clickable(onClick = onClick)
            .padding(vertical = 10.dp, horizontal = 4.dp),
    ) {
        Text(
            title,
            color = MaterialTheme.colorScheme.onSurface,
            fontWeight = FW.SemiBold,
            style = MaterialTheme.typography.bodyLarge,
        )
        Text(
            hint,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            style = MaterialTheme.typography.bodySmall,
        )
    }
}
