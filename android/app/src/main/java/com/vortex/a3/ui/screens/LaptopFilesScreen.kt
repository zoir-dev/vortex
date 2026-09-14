package com.vortex.a3.ui.screens

import android.os.Environment
import android.widget.Toast
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material.icons.outlined.Description
import androidx.compose.material.icons.outlined.Folder
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import com.vortex.a3.core.fs.FsClient
import com.vortex.a3.core.fs.FsCode
import com.vortex.a3.core.fs.FsEntry
import java.io.File
import kotlinx.coroutines.launch

/**
 * Browse the laptop's shared folders and pull files down.
 *
 * Deliberately thin: everything it shows comes from [FsClient], and the laptop
 * decides what is visible through its own roots config. This screen never
 * constructs a path — it sends back the opaque `path` of an entry it was given,
 * which is what lets the same code work whether the far side is a real
 * filesystem or something else entirely.
 */
@Composable
fun LaptopFilesScreen(onBack: () -> Unit) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()

    // The trail of folders we descended, so Back walks up one level rather than
    // leaving the screen from three levels deep.
    var stack by remember { mutableStateOf(listOf<Pair<String, String>>()) } // (path, title)
    var entries by remember { mutableStateOf<List<FsEntry>>(emptyList()) }
    var loading by remember { mutableStateOf(true) }
    var error by remember { mutableStateOf<String?>(null) }
    var downloading by remember { mutableStateOf<String?>(null) }
    var progress by remember { mutableStateOf(0f) }

    val path = stack.lastOrNull()?.first ?: ""
    val title = stack.lastOrNull()?.second ?: "Laptop files"

    LaunchedEffect(path) {
        loading = true
        error = null
        try {
            entries = FsClient.listAll(path).sortedWith(
                // Folders first, then case-insensitive by name — what every
                // file manager does, and cheap to do here rather than asking
                // the far side to sort.
                compareBy({ !it.isDir }, { it.name.lowercase() }),
            )
        } catch (e: FsClient.FsException) {
            entries = emptyList()
            error = explain(e)
        } catch (e: Exception) {
            entries = emptyList()
            error = e.message ?: "Could not read that folder"
        }
        loading = false
    }

    // Back — gesture or button — walks UP one folder and only leaves the screen
    // from the top. Registered here rather than in the caller (as Notes and
    // Settings do) precisely because it is not a plain dismiss: the caller does
    // not know how deep the browse is.
    BackHandler {
        if (stack.isNotEmpty()) stack = stack.dropLast(1) else onBack()
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(MaterialTheme.colorScheme.background)
            // targetSdk 36 makes edge-to-edge mandatory, and nothing in this app
            // compensates — so without this the header draws UNDER the status
            // bar: the back arrow lands behind the clock, where it is hard to
            // see and hard to hit (a synthetic tap on it is swallowed
            // outright). The background is applied before the padding so the
            // bar still sits on our colour rather than a bare gap.
            .systemBarsPadding(),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            // Same action as the back gesture, so the two never disagree about
            // what "back" means at a given depth.
            IconButton(onClick = { if (stack.isNotEmpty()) stack = stack.dropLast(1) else onBack() }) {
                Icon(
                    Icons.AutoMirrored.Outlined.ArrowBack,
                    contentDescription = null,
                    tint = MaterialTheme.colorScheme.onSurface,
                )
            }
            Text(
                title,
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.SemiBold,
                color = MaterialTheme.colorScheme.onSurface,
            )
        }

        if (downloading != null) {
            Column(modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp)) {
                Text(
                    "Downloading ${downloading}",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.size(6.dp))
                // Indeterminate when the laptop did not report a size: a bar
                // stuck at 0% would read as broken.
                if (progress >= 0f) {
                    LinearProgressIndicator(
                        progress = { progress },
                        modifier = Modifier.fillMaxWidth(),
                    )
                } else {
                    LinearProgressIndicator(modifier = Modifier.fillMaxWidth())
                }
            }
        }

        when {
            loading -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                CircularProgressIndicator()
            }
            error != null -> Box(
                Modifier.fillMaxSize().padding(32.dp),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    error!!,
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            entries.isEmpty() -> Box(
                Modifier.fillMaxSize().padding(32.dp),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    "This folder is empty",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            else -> LazyColumn(modifier = Modifier.fillMaxSize()) {
                items(entries, key = { it.path.ifEmpty { it.name } }) { e ->
                    EntryRow(e) {
                        if (e.isDir) {
                            stack = stack + (e.path to e.name)
                        } else if (downloading == null) {
                            downloading = e.name
                            progress = if (e.size > 0) 0f else -1f
                            scope.launch {
                                val dest = File(
                                    Environment.getExternalStoragePublicDirectory(
                                        Environment.DIRECTORY_DOWNLOADS,
                                    ),
                                    uniqueName(e.name),
                                )
                                val msg = try {
                                    FsClient.download(e.path, dest) { done, total ->
                                        progress = if (total > 0) {
                                            (done.toDouble() / total).toFloat()
                                        } else {
                                            -1f
                                        }
                                    }
                                    "Saved to Downloads/${dest.name}"
                                } catch (ex: FsClient.FsException) {
                                    explain(ex)
                                } catch (ex: Exception) {
                                    ex.message ?: "Download failed"
                                }
                                downloading = null
                                Toast.makeText(context, msg, Toast.LENGTH_LONG).show()
                            }
                        }
                    }
                    HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
                }
            }
        }
    }
}

@Composable
private fun EntryRow(e: FsEntry, onClick: () -> Unit) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(horizontal = 16.dp, vertical = 14.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            if (e.isDir) Icons.Outlined.Folder else Icons.Outlined.Description,
            contentDescription = null,
            tint = MaterialTheme.colorScheme.primary,
            modifier = Modifier.size(22.dp),
        )
        Spacer(Modifier.width(14.dp))
        Column(modifier = Modifier.fillMaxWidth(), verticalArrangement = Arrangement.Center) {
            Text(
                e.name,
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurface,
            )
            if (!e.isDir) {
                Text(
                    humanSize(e.size),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}

/** Say what actually went wrong. The codes are errno-shaped, so each one has a
 *  true sentence — "not permitted" and "the laptop did not answer" send the
 *  user to completely different places. */
private fun explain(e: FsClient.FsException): String = when (e.code) {
    FsCode.NOENT -> "That file is no longer there"
    FsCode.ACCES -> "The laptop is not sharing that folder"
    FsCode.NOTSUP -> "The laptop does not support that"
    FsCode.ISDIR -> "That is a folder"
    FsClient.TIMEOUT -> "The laptop did not answer — is it awake and in range?"
    else -> "Could not read that (${e.message})"
}

private fun humanSize(bytes: Long): String = when {
    bytes < 1024 -> "$bytes B"
    bytes < 1024 * 1024 -> "%.0f KB".format(bytes / 1024.0)
    bytes < 1024L * 1024 * 1024 -> "%.1f MB".format(bytes / (1024.0 * 1024))
    else -> "%.2f GB".format(bytes / (1024.0 * 1024 * 1024))
}

/** Never overwrite something already in Downloads: append " (n)" like every
 *  browser does, so a second pull of the same name is not a silent loss. */
private fun uniqueName(name: String): String {
    val dir = Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS)
    if (!File(dir, name).exists()) return name
    val dot = name.lastIndexOf('.')
    val stem = if (dot > 0) name.substring(0, dot) else name
    val ext = if (dot > 0) name.substring(dot) else ""
    var i = 1
    while (File(dir, "$stem ($i)$ext").exists()) i++
    return "$stem ($i)$ext"
}
