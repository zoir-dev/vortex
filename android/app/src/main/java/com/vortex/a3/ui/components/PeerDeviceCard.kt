package com.vortex.a3.ui.components

import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Cast
import androidx.compose.material.icons.outlined.PhonelinkOff
import androidx.compose.material.icons.outlined.Lock
import androidx.compose.material.icons.outlined.LockOpen
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.ripple
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.scale
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.font.FontWeight as FW
import androidx.compose.ui.unit.dp

/**
 * The "paired Linux laptop" card on the home screen. Body is just
 * name + caption + battery; long-press surfaces the Forget dialog.
 * When the peer reports a lock-screen state ([locked] != null) a small
 * lock/unlock action sits next to the battery row — tap locks the
 * laptop, or (behind a biometric confirm upstream) unlocks it.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun PeerDeviceCard(
    modifier: Modifier = Modifier,
    icon: ImageVector,
    name: String,
    caption: String,
    battery: Int?,
    charging: Boolean = false,
    statusDotColor: Color,
    onLongPress: () -> Unit,
    locked: Boolean? = null,
    onToggleLock: (() -> Unit)? = null,
    /** Tap to view the laptop's screen on this phone (laptop→phone mirror).
     *  Null hides the action (e.g. the peer isn't a laptop / not reachable). */
    onViewScreen: (() -> Unit)? = null,
    /** Tap to look for another remembered laptop while staying on this one.
     *  Null hides the action (fewer than two laptops remembered). */
    onSwitch: (() -> Unit)? = null,
    /** True while a seek window is open — shows the action as busy. */
    seeking: Boolean = false,
) {
    val interaction = remember { MutableInteractionSource() }
    val pressed by interaction.collectIsPressedAsState()
    val scale by animateFloatAsState(
        targetValue = if (pressed) 0.97f else 1f,
        animationSpec = spring(dampingRatio = Spring.DampingRatioMediumBouncy, stiffness = Spring.StiffnessMediumLow),
        label = "card-press",
    )

    // Note ordering: shadow → clip → border → background → click.
    // The clip MUST come before combinedClickable so the ripple is
    // bounded by the rounded corners.
    Column(
        modifier = modifier
            .scale(scale)
            .height(CardHeight)
            .shadow(elevation = 6.dp, shape = CardCorner, clip = false)
            .clip(CardCorner)
            .background(MaterialTheme.colorScheme.surface)
            .combinedClickable(
                interactionSource = interaction,
                indication = ripple(bounded = true, color = MaterialTheme.colorScheme.primary.copy(alpha = 0.4f)),
                onClick = {},
                onLongClick = onLongPress,
            )
            .border(width = 1.dp, color = MaterialTheme.colorScheme.outline, shape = CardCorner)
            .padding(16.dp),
    ) {
        CardHeader(
            icon = icon,
            iconTint = MaterialTheme.colorScheme.primary,
            iconBg = MaterialTheme.colorScheme.primary.copy(alpha = 0.15f),
            statusDot = statusDotColor,
            // Beside the device icon, not in the bottom row. Two facing arrows
            // sandwiched between the cast and lock glyphs read as "swap those
            // two", and it crowded the row enough to wrap the battery
            // percentage onto a second line. "Unlink" says what this does:
            // leave this laptop for another one.
            afterIcon = onSwitch?.let { switch ->
                {
                    Icon(
                        imageVector = Icons.Outlined.PhonelinkOff,
                        contentDescription = "Switch to another laptop",
                        tint = if (seeking) {
                            MaterialTheme.colorScheme.primary
                        } else {
                            MaterialTheme.colorScheme.onSurfaceVariant
                        },
                        modifier = Modifier
                            .clip(RoundedCornerShape(8.dp))
                            .clickable(onClick = switch)
                            .padding(4.dp)
                            .size(20.dp),
                    )
                }
            },
        )
        Spacer(modifier = Modifier.height(14.dp))
        Text(name, color = MaterialTheme.colorScheme.onSurface, fontWeight = FW.SemiBold, style = MaterialTheme.typography.bodyLarge, maxLines = 1)
        Text(caption, color = MaterialTheme.colorScheme.onSurfaceVariant, style = MaterialTheme.typography.bodySmall)
        // Pushes the battery row to the bottom regardless of content above.
        Spacer(modifier = Modifier.weight(1f))
        Row(verticalAlignment = Alignment.CenterVertically) {
            Box(modifier = Modifier.weight(1f)) {
                BatteryRow(battery, charging = charging)
            }
            if (onViewScreen != null) {
                // View laptop screen (laptop→phone mirror) ships Experimental in
                // v1 — a small amber dot flags it as a beta feature.
                Box(contentAlignment = Alignment.TopEnd) {
                    Icon(
                        imageVector = Icons.Outlined.Cast,
                        contentDescription = "View laptop screen (experimental)",
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier
                            .clip(RoundedCornerShape(8.dp))
                            .clickable(onClick = onViewScreen)
                            .padding(4.dp)
                            .size(20.dp),
                    )
                    Box(
                        Modifier
                            .padding(top = 3.dp, end = 2.dp)
                            .size(7.dp)
                            .clip(RoundedCornerShape(percent = 50))
                            .background(Color(0xFFF0B43C)),
                    )
                }
                Spacer(modifier = Modifier.size(8.dp))
            }
            if (locked != null && onToggleLock != null) {
                Icon(
                    imageVector = if (locked) Icons.Outlined.Lock else Icons.Outlined.LockOpen,
                    contentDescription = null,
                    tint = if (locked) {
                        MaterialTheme.colorScheme.primary
                    } else {
                        MaterialTheme.colorScheme.onSurfaceVariant
                    },
                    modifier = Modifier
                        .clip(RoundedCornerShape(8.dp))
                        .clickable(onClick = onToggleLock)
                        .padding(4.dp)
                        .size(20.dp),
                )
            }
        }
    }
}
