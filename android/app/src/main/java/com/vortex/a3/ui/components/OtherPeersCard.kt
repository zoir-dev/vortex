package com.vortex.a3.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Laptop
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.text.font.FontWeight as FW
import androidx.compose.ui.unit.dp
import com.vortex.a3.core.storage.TrustedPeer
import com.vortex.a3.ui.str

/**
 * The laptops this phone is paired with but not currently using.
 *
 * Compact rows rather than full [PeerDeviceCard]s: those are 180 dp each, so a
 * card per laptop would push everything else off the screen for information
 * that is mostly "this one exists and is not the one you are on".
 *
 * Tapping a row switches to THAT laptop. That is deliberately more specific
 * than the card header's generic unlink action: naming the destination lets the
 * seek advertise a single token instead of cycling every remembered peer, so it
 * is both faster and cheaper on air (design doc §D1).
 */
@Composable
fun OtherPeersCard(
    peers: List<TrustedPeer>,
    lastSeen: Map<String, Long>,
    now: Long,
    seeking: Boolean,
    onSwitchTo: (TrustedPeer) -> Unit,
) {
    if (peers.isEmpty()) return
    SurfaceCard {
        Text(
            str("peers.other_title"),
            color = MaterialTheme.colorScheme.onSurface,
            fontWeight = FW.SemiBold,
            style = MaterialTheme.typography.titleSmall,
        )
        Spacer(modifier = Modifier.height(4.dp))
        Text(
            str("peers.other_hint"),
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            style = MaterialTheme.typography.bodySmall,
        )
        for (peer in peers) {
            Spacer(modifier = Modifier.height(10.dp))
            val hex = peer.peerStaticPub.toHex()
            val seen = lastSeen[hex] ?: 0L
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .clip(RoundedCornerShape(10.dp))
                    // The whole row is the target: a 20 dp glyph is a poor tap
                    // area, and there is only one action per row anyway.
                    .clickable(enabled = !seeking) { onSwitchTo(peer) }
                    .padding(vertical = 6.dp, horizontal = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                Box(
                    modifier = Modifier
                        .size(34.dp)
                        .clip(RoundedCornerShape(10.dp))
                        .background(
                            MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.12f),
                        ),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(
                        imageVector = Icons.Outlined.Laptop,
                        contentDescription = null,
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.size(18.dp),
                    )
                }
                Column(modifier = Modifier.weight(1f)) {
                    Text(
                        peer.peerName?.takeIf { it.isNotBlank() } ?: str("device.linux"),
                        color = MaterialTheme.colorScheme.onSurface,
                        style = MaterialTheme.typography.bodyMedium,
                        maxLines = 1,
                    )
                    Text(
                        // Never claim a peer is reachable: these are the ones we
                        // are NOT connected to, so the honest thing to show is
                        // when it was last heard from.
                        lastSeenLabel(seen, peer.pairedAt, now),
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        style = MaterialTheme.typography.bodySmall,
                        maxLines = 1,
                    )
                }
                // No trailing "Switch" label: the row IS the button and the
                // heading already says so, so a per-row repeat is noise. Only
                // the in-flight spinner earns space here.
                if (seeking) {
                    CircularProgressIndicator(
                        modifier = Modifier.size(16.dp),
                        strokeWidth = 2.dp,
                        color = MaterialTheme.colorScheme.primary,
                    )
                }
            }
        }
    }
}

/**
 * "seen 12 min ago", or "paired 2 d ago" when we have not heard from it.
 *
 * [seenMs] only covers peers heard from during THIS app process — it is not
 * persisted — so a laptop paired yesterday reads as never-seen after a restart.
 * "not seen yet" would be true of the session and false to the user, who
 * remembers pairing it. Falling back to [pairedAtSec] says something both
 * accurate and useful.
 */
private fun lastSeenLabel(seenMs: Long, pairedAtSec: Long, nowMs: Long): String {
    if (seenMs > 0L) return "seen ${ago((nowMs - seenMs) / 1000)}"
    if (pairedAtSec > 0L) return "paired ${ago(nowMs / 1000 - pairedAtSec)}"
    return "not connected"
}

private fun ago(secs: Long): String {
    val s = secs.coerceAtLeast(0)
    return when {
        s < 60 -> "just now"
        s < 3600 -> "${s / 60} min ago"
        s < 86_400 -> "${s / 3600} h ago"
        else -> "${s / 86_400} d ago"
    }
}
