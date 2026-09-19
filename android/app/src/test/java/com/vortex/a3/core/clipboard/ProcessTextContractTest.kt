package com.vortex.a3.core.clipboard

import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import java.io.File

/**
 * Static guards on the text-selection-toolbar entry point.
 *
 * Both checks are for mistakes that are invisible in review and only show up
 * on a device: one silently removes Vortex from the toolbar, the other
 * silently edits the user's document.
 */
class ProcessTextContractTest {

    private val manifest = File("src/main/AndroidManifest.xml")
    private val activity =
        File("src/main/java/com/vortex/a3/core/clipboard/ProcessTextActivity.kt")

    @Test
    fun `the toolbar entry point is declared and reachable`() {
        val xml = manifest.readText()
        assertTrue(xml.contains("ProcessTextActivity"), "activity missing from the manifest")
        assertTrue(
            xml.contains("android.intent.action.PROCESS_TEXT"),
            "no PROCESS_TEXT filter — Vortex would not appear in the selection toolbar",
        )
        // Not exported = the system cannot launch it, and the toolbar item
        // silently never appears.
        val block = xml.substringAfter("ProcessTextActivity").substringBefore("</activity>")
        assertTrue(
            block.contains("android:exported=\"true\""),
            "the activity must be exported for the system to launch it",
        )
        assertTrue(
            block.contains("android.intent.category.DEFAULT"),
            "an implicit intent needs category DEFAULT to resolve",
        )
    }

    /**
     * Returning `EXTRA_PROCESS_TEXT` in the result makes the HOST app replace
     * the user's selection with whatever we send back. Sending a copy to the
     * laptop must never edit what they were reading.
     */
    @Test
    fun `the activity never writes back over the user's selection`() {
        val src = activity.readText()
        assertFalse(
            src.contains("setResult("),
            "setResult would overwrite the selected text in the host app",
        )
    }
}
