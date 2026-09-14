package com.vortex.a3.ui.screens

import androidx.compose.animation.core.animateDpAsState
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.ArrowBack
import androidx.compose.material.icons.outlined.ContentPaste
import androidx.compose.material.icons.outlined.DarkMode
import androidx.compose.material.icons.outlined.FileDownload
import androidx.compose.material.icons.outlined.FolderOpen
import androidx.compose.material.icons.outlined.Movie
import androidx.compose.material.icons.outlined.Storage
import androidx.compose.material.icons.outlined.Headset
import androidx.compose.material.icons.outlined.Language
import androidx.compose.material.icons.outlined.LightMode
import androidx.compose.material.icons.outlined.Notifications
import androidx.compose.material.icons.outlined.NotificationsActive
import androidx.compose.material.icons.outlined.PhotoCamera
import androidx.compose.material.icons.outlined.Screenshot
import androidx.compose.material.icons.outlined.TouchApp
import androidx.compose.material.icons.outlined.Videocam
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight as FW
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.vortex.a3.ui.ThemeMode
import com.vortex.a3.ui.VortexLocale
import com.vortex.a3.ui.components.VortexDivider
import com.vortex.a3.ui.str

/**
 * Full-screen Settings page, grouped into section cards (Appearance /
 * Continuity / Device) to mirror the laptop's redesigned Settings: an
 * uppercase section label over a single card whose rows are split by dividers,
 * each row led by a rounded icon tile.
 */
@Composable
fun SettingsScreen(
    current: VortexLocale,
    onSelect: (VortexLocale) -> Unit,
    currentTheme: ThemeMode,
    onSelectTheme: (ThemeMode) -> Unit,
    smartSwitchOn: Boolean,
    onSmartSwitchChange: (Boolean) -> Unit,
    notifMirrorOn: Boolean,
    onNotifMirrorChange: (Boolean) -> Unit,
    peerNotifShowOn: Boolean,
    onPeerNotifShowChange: (Boolean) -> Unit,
    clipboardSyncOn: Boolean,
    onClipboardSyncChange: (Boolean) -> Unit,
    clipboardAutoGranted: Boolean,
    fileAutoAcceptOn: Boolean,
    onFileAutoAcceptChange: (Boolean) -> Unit,
    shareScreenshotsOn: Boolean,
    onShareScreenshotsChange: (Boolean) -> Unit,
    sharePhotosOn: Boolean,
    onSharePhotosChange: (Boolean) -> Unit,
    shareScreenRecordingsOn: Boolean,
    onShareScreenRecordingsChange: (Boolean) -> Unit,
    shareVideosOn: Boolean,
    onShareVideosChange: (Boolean) -> Unit,
    /** Whether the media grants behind the four rows above are held; without
     *  them their hint says what to do instead of what they do. */
    mediaReadGranted: Boolean,
    /** How many folders the laptop may browse; 0 hides nothing, it just makes
     *  the row say there is nothing to browse yet. */
    sharedFolderCount: Int,
    onPickSharedFolder: () -> Unit,
    screenControlOn: Boolean,
    onScreenControlClick: () -> Unit,
    allFilesOn: Boolean,
    onAllFilesClick: () -> Unit,
    onBack: () -> Unit,
) {
    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(MaterialTheme.colorScheme.background)
            // targetSdk 36 makes edge-to-edge mandatory: the app draws behind
            // the status bar whether it asks to or not, so this header sat in
            // the same band as the clock, where the system consumes the touch.
            // The back arrow rendered fine and simply did not respond, which
            // reads as a broken button rather than a mispositioned one.
            // Background BEFORE padding, so the status bar still sits on our
            // colour instead of a bare strip.
            .systemBarsPadding(),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = onBack) {
                Icon(Icons.Outlined.ArrowBack, contentDescription = null, tint = MaterialTheme.colorScheme.onSurface)
            }
            Text(
                str("settings.title"),
                color = MaterialTheme.colorScheme.onSurface,
                fontWeight = FW.SemiBold,
                style = MaterialTheme.typography.titleMedium,
                modifier = Modifier.padding(start = 4.dp),
            )
        }
        VortexDivider()

        Column(
            modifier = Modifier
                .fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 16.dp)
                .padding(bottom = 24.dp),
        ) {
            // ── APPEARANCE ──────────────────────────────────────────────
            SectionLabel(str("settings.sec_appearance"))
            SectionCard {
                PickerRow(icon = Icons.Outlined.Language, label = str("settings.language")) {
                    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        for (loc in VortexLocale.entries) {
                            SegmentedButton(
                                label = loc.label,
                                selected = loc == current,
                                onClick = { onSelect(loc) },
                                modifier = Modifier.weight(1f),
                            )
                        }
                    }
                }
                RowDivider()
                PickerRow(
                    icon = if (currentTheme == ThemeMode.Light) Icons.Outlined.LightMode else Icons.Outlined.DarkMode,
                    label = str("settings.theme"),
                ) {
                    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        SegmentedButton(
                            label = str("settings.theme_dark"),
                            selected = currentTheme == ThemeMode.Dark,
                            onClick = { onSelectTheme(ThemeMode.Dark) },
                            leadingIcon = Icons.Outlined.DarkMode,
                            modifier = Modifier.weight(1f),
                        )
                        SegmentedButton(
                            label = str("settings.theme_light"),
                            selected = currentTheme == ThemeMode.Light,
                            onClick = { onSelectTheme(ThemeMode.Light) },
                            leadingIcon = Icons.Outlined.LightMode,
                            modifier = Modifier.weight(1f),
                        )
                    }
                }
            }

            // ── CONTINUITY ──────────────────────────────────────────────
            SectionLabel(str("settings.sec_continuity"))
            SectionCard {
                ToggleRow(
                    icon = Icons.Outlined.Headset,
                    title = str("settings.smart_switch"),
                    hint = str("settings.smart_switch_hint"),
                    checked = smartSwitchOn,
                    onCheckedChange = onSmartSwitchChange,
                )
                RowDivider()
                ToggleRow(
                    icon = Icons.Outlined.Notifications,
                    title = str("settings.notif_mirror"),
                    hint = str("settings.notif_mirror_hint"),
                    checked = notifMirrorOn,
                    onCheckedChange = onNotifMirrorChange,
                )
                RowDivider()
                ToggleRow(
                    icon = Icons.Outlined.NotificationsActive,
                    title = str("settings.peer_notif"),
                    hint = str("settings.peer_notif_hint"),
                    checked = peerNotifShowOn,
                    onCheckedChange = onPeerNotifShowChange,
                )
                RowDivider()
                ToggleRow(
                    icon = Icons.Outlined.ContentPaste,
                    title = str("settings.clipboard_sync"),
                    hint = if (clipboardAutoGranted) str("settings.clipboard_auto_on")
                    else str("settings.clipboard_sync_hint"),
                    checked = clipboardSyncOn,
                    onCheckedChange = onClipboardSyncChange,
                )
                RowDivider()
                ToggleRow(
                    icon = Icons.Outlined.FileDownload,
                    title = str("settings.file_auto_accept"),
                    hint = str("settings.file_auto_accept_hint"),
                    checked = fileAutoAcceptOn,
                    onCheckedChange = onFileAutoAcceptChange,
                )
            }

            // ── PHONE → LAPTOP ──────────────────────────────────────────
            // Two rows, both off until switched on: a screenshot and a photo
            // are different decisions (see MediaAutoShareSetting). The grant
            // is asked for on the first flip; until it is held the hint says
            // so in place of the description.
            SectionLabel(str("settings.sec_auto_share"))
            SectionCard {
                ToggleRow(
                    icon = Icons.Outlined.Screenshot,
                    title = str("settings.share_screenshots"),
                    hint = if (mediaReadGranted || !shareScreenshotsOn) str("settings.share_screenshots_hint")
                    else str("settings.share_media_needs_permission"),
                    checked = shareScreenshotsOn,
                    onCheckedChange = onShareScreenshotsChange,
                )
                RowDivider()
                ToggleRow(
                    icon = Icons.Outlined.PhotoCamera,
                    title = str("settings.share_photos"),
                    hint = if (mediaReadGranted || !sharePhotosOn) str("settings.share_photos_hint")
                    else str("settings.share_media_needs_permission"),
                    checked = sharePhotosOn,
                    onCheckedChange = onSharePhotosChange,
                )
                RowDivider()
                ToggleRow(
                    icon = Icons.Outlined.Videocam,
                    title = str("settings.share_screen_recordings"),
                    hint = if (mediaReadGranted || !shareScreenRecordingsOn) {
                        str("settings.share_screen_recordings_hint")
                    } else {
                        str("settings.share_media_needs_permission")
                    },
                    checked = shareScreenRecordingsOn,
                    onCheckedChange = onShareScreenRecordingsChange,
                )
                RowDivider()
                ToggleRow(
                    icon = Icons.Outlined.Movie,
                    title = str("settings.share_videos"),
                    hint = if (mediaReadGranted || !shareVideosOn) str("settings.share_videos_hint")
                    else str("settings.share_media_needs_permission"),
                    checked = shareVideosOn,
                    onCheckedChange = onShareVideosChange,
                )
            }

            // ── LAPTOP → PHONE, the other way round ──────────────────────
            // Not a toggle: nothing is copied by this. The laptop gets to LIST
            // a folder the user hands it and pull a file out on request, which
            // is what a Download folder actually wants — mostly things nobody
            // needs a second copy of, and only the person looking knows which
            // few are the exception.
            SectionLabel(str("settings.sec_browse"))
            SectionCard {
                ActionRow(
                    icon = Icons.Outlined.FolderOpen,
                    title = str("settings.shared_folders"),
                    hint = if (sharedFolderCount > 0) {
                        str("settings.shared_folders_some").replace("%d", "$sharedFolderCount")
                    } else {
                        str("settings.shared_folders_none")
                    },
                    status = if (sharedFolderCount > 0) "$sharedFolderCount" else "",
                    onClick = onPickSharedFolder,
                )
            }
            // When sync is on but background reads aren't granted, phone→laptop
            // needs the QS tile — show the one-time ADB grant command.
            if (clipboardSyncOn && !clipboardAutoGranted) {
                Spacer(Modifier.height(10.dp))
                AdbHintCard(
                    title = str("settings.clipboard_adb_title"),
                    body = str("settings.clipboard_adb_body"),
                    command = "adb shell appops set ${com.vortex.a3.BuildConfig.APPLICATION_ID} READ_CLIPBOARD allow",
                )
            }

            // ── DEVICE ──────────────────────────────────────────────────
            SectionLabel(str("settings.sec_device"))
            SectionCard {
                ActionRow(
                    icon = Icons.Outlined.TouchApp,
                    title = "Screen control",
                    hint = if (screenControlOn) "On — the laptop can control this phone while mirroring"
                    else "Off — tap to enable in Accessibility",
                    status = if (screenControlOn) "On" else "Off",
                    onClick = onScreenControlClick,
                )
                ActionRow(
                    icon = Icons.Outlined.Storage,
                    title = "Allow access to any files",
                    // Says what it costs before it is granted, and what it
                    // replaces once it is: with all-files on, the picked
                    // folders are superseded rather than added to, and showing
                    // the same file under two paths would be worse than saying
                    // so here.
                    hint = if (allFilesOn)
                        "On — the laptop can browse all of your storage, read-only"
                    else "Off — instead of picking folders, share everything (asks Android)",
                    status = if (allFilesOn) "On" else "Off",
                    onClick = onAllFilesClick,
                )
            }
        }
    }
}

/** Uppercase muted section label over a card. */
@Composable
private fun SectionLabel(text: String) {
    Text(
        text.uppercase(),
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        style = MaterialTheme.typography.labelSmall,
        fontWeight = FW.SemiBold,
        letterSpacing = 1.sp,
        modifier = Modifier.padding(start = 6.dp, top = 18.dp, bottom = 8.dp),
    )
}

/** A single card whose rows are split by [RowDivider]s. */
@Composable
private fun SectionCard(content: @Composable ColumnScope.() -> Unit) {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(16.dp))
            .background(MaterialTheme.colorScheme.surface)
            .border(1.dp, MaterialTheme.colorScheme.outline, RoundedCornerShape(16.dp)),
        content = content,
    )
}

@Composable
private fun RowDivider() {
    HorizontalDivider(thickness = 1.dp, color = MaterialTheme.colorScheme.outline.copy(alpha = 0.4f))
}

/** A 36dp rounded icon tile leading each row (matches the laptop). */
@Composable
private fun IconTile(icon: ImageVector) {
    Box(
        Modifier
            .size(36.dp)
            .clip(RoundedCornerShape(10.dp))
            .background(MaterialTheme.colorScheme.onSurface.copy(alpha = 0.05f))
            .border(1.dp, MaterialTheme.colorScheme.outline.copy(alpha = 0.6f), RoundedCornerShape(10.dp)),
        contentAlignment = Alignment.Center,
    ) {
        Icon(icon, null, tint = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.size(19.dp))
    }
}

/** A picker row inside a section card: small icon + label, then content below
 *  (segmented buttons for language / theme). */
@Composable
private fun PickerRow(icon: ImageVector, label: String, content: @Composable () -> Unit) {
    Column(
        Modifier.fillMaxWidth().padding(14.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Icon(icon, null, tint = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.size(19.dp))
            Text(label, color = MaterialTheme.colorScheme.onSurface, fontWeight = FW.SemiBold, style = MaterialTheme.typography.bodyMedium)
        }
        content()
    }
}

/** A boolean setting row inside a section card: icon tile + title + hint, an
 *  iOS-style switch on the right; the whole row toggles. */
@Composable
private fun ToggleRow(
    icon: ImageVector,
    title: String,
    hint: String?,
    checked: Boolean,
    onCheckedChange: (Boolean) -> Unit,
) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable { onCheckedChange(!checked) }
            .padding(14.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        IconTile(icon)
        Column(modifier = Modifier.weight(1f)) {
            Text(title, color = MaterialTheme.colorScheme.onSurface, fontWeight = FW.SemiBold, style = MaterialTheme.typography.bodyMedium)
            if (!hint.isNullOrEmpty()) {
                Text(hint, color = MaterialTheme.colorScheme.onSurfaceVariant, style = MaterialTheme.typography.bodySmall, modifier = Modifier.padding(top = 2.dp))
            }
        }
        IosSwitch(checked = checked, onCheckedChange = onCheckedChange)
    }
}

/** A tappable row that opens a system screen (no inline toggle) — a status chip
 *  instead of a switch. */
@Composable
private fun ActionRow(icon: ImageVector, title: String, hint: String?, status: String, onClick: () -> Unit) {
    Row(
        modifier = Modifier.fillMaxWidth().clickable { onClick() }.padding(14.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        IconTile(icon)
        Column(modifier = Modifier.weight(1f)) {
            Text(title, color = MaterialTheme.colorScheme.onSurface, fontWeight = FW.SemiBold, style = MaterialTheme.typography.bodyMedium)
            if (hint != null) {
                Text(hint, color = MaterialTheme.colorScheme.onSurfaceVariant, style = MaterialTheme.typography.bodySmall, modifier = Modifier.padding(top = 2.dp))
            }
        }
        Text(status, color = MaterialTheme.colorScheme.onSurfaceVariant, fontWeight = FW.Medium, style = MaterialTheme.typography.bodySmall)
    }
}

/** An info card explaining a one-time ADB grant, with the command in a
 *  monospaced, selectable box. */
@Composable
private fun AdbHintCard(title: String, body: String, command: String) {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(16.dp))
            .background(MaterialTheme.colorScheme.surface)
            .border(1.dp, MaterialTheme.colorScheme.outline, RoundedCornerShape(16.dp))
            .padding(14.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text(title, color = MaterialTheme.colorScheme.onSurface, fontWeight = FW.Medium, style = MaterialTheme.typography.bodyMedium)
        Text(body, color = MaterialTheme.colorScheme.onSurfaceVariant, style = MaterialTheme.typography.bodySmall)
        Text(
            command,
            color = MaterialTheme.colorScheme.onSurface,
            fontFamily = FontFamily.Monospace,
            style = MaterialTheme.typography.bodySmall,
            modifier = Modifier
                .fillMaxWidth()
                .background(MaterialTheme.colorScheme.background, RoundedCornerShape(8.dp))
                .padding(horizontal = 10.dp, vertical = 8.dp),
        )
    }
}

/**
 * Compact iOS-style switch matching the laptop's ToggleRow.vue: a 44×24 track
 * (emerald when on, muted when off) with a 20dp white knob that slides.
 */
@Composable
private fun IosSwitch(checked: Boolean, onCheckedChange: (Boolean) -> Unit) {
    val knobX by animateDpAsState(if (checked) 22.dp else 2.dp, label = "knob")
    val track = if (checked) Color(0xFF10B981) else MaterialTheme.colorScheme.onSurface.copy(alpha = 0.28f)
    Box(
        modifier = Modifier
            .width(44.dp)
            .height(24.dp)
            .clip(RoundedCornerShape(12.dp))
            .background(track)
            .clickable { onCheckedChange(!checked) },
    ) {
        Box(
            modifier = Modifier
                .offset(x = knobX, y = 2.dp)
                .size(20.dp)
                .clip(CircleShape)
                .background(Color.White),
        )
    }
}

@Composable
private fun SegmentedButton(
    label: String,
    selected: Boolean,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    leadingIcon: ImageVector? = null,
    enabled: Boolean = true,
) {
    val bg = if (selected) MaterialTheme.colorScheme.primary else Color.Transparent
    val fg = when {
        !enabled -> MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.5f)
        selected -> MaterialTheme.colorScheme.onPrimary
        else -> MaterialTheme.colorScheme.onSurface
    }
    val borderColor = if (selected) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.outline
    Row(
        modifier = modifier
            .background(bg, RoundedCornerShape(8.dp))
            .border(width = 1.dp, color = borderColor, shape = RoundedCornerShape(8.dp))
            .height(36.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.Center,
    ) {
        TextButton(
            onClick = onClick,
            enabled = enabled,
            contentPadding = PaddingValues(horizontal = 8.dp, vertical = 0.dp),
            modifier = Modifier.fillMaxWidth(),
        ) {
            if (leadingIcon != null) {
                Icon(imageVector = leadingIcon, contentDescription = null, tint = fg, modifier = Modifier.size(14.dp))
                Spacer(modifier = Modifier.width(4.dp))
            }
            Text(label, color = fg, style = MaterialTheme.typography.bodySmall, fontWeight = if (selected) FW.SemiBold else FW.Normal)
        }
    }
}
