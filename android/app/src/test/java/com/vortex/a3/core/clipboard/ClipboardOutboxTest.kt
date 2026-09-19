package com.vortex.a3.core.clipboard

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.BeforeEach
import org.junit.jupiter.api.Test

/**
 * The LAN fallback for clipboard text the BLE notify could not deliver.
 *
 * Phone→laptop clipboard text used to be a BLE notify and nothing else, so a
 * laptop with a dead GATT link lost every copy in silence. These pin the two
 * properties the fallback relies on.
 */
class ClipboardOutboxTest {

    @BeforeEach
    fun reset() = ClipboardOutbox.clear()

    @Test
    fun `text survives until it is taken, then the slot is empty`() {
        ClipboardOutbox.stash("copied on the phone")
        assertEquals("copied on the phone", ClipboardOutbox.take())
        // Taken once: a second bulk-sync round must not re-deliver it and
        // overwrite whatever the user has copied since.
        assertNull(ClipboardOutbox.take())
    }

    @Test
    fun `latest copy wins over an undelivered older one`() {
        ClipboardOutbox.stash("first")
        ClipboardOutbox.stash("second")
        // A clipboard holds one item; delivering "first" would paste content
        // the user has already replaced.
        assertEquals("second", ClipboardOutbox.take())
    }

    @Test
    fun `nothing pending yields nothing`() {
        assertNull(ClipboardOutbox.take())
    }

    @Test
    fun `an empty copy is never stashed`() {
        ClipboardOutbox.stash("")
        assertNull(ClipboardOutbox.take())
    }
}
