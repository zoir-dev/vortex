package com.vortex.a3.core.lan

import android.content.Context
import android.content.SharedPreferences
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * Whether files pushed from the paired laptop are saved without asking.
 *
 * LOCAL-only (like [com.vortex.a3.core.clipboard.ClipboardSyncSetting]) — the
 * laptop has its own, independent switch for the other direction, because
 * "I trust files landing on my phone" and "I trust files landing on my laptop"
 * are separate decisions and must not be coupled.
 *
 * Default **OFF**: this removes the [FileConsent] gate, so it only ever turns
 * on because the user asked for it. Process-wide singleton over SharedPreferences
 * `vortex_ui_settings` (the same store the other UI settings use).
 */
object FileAutoAcceptSetting {
    private const val PREFS = "vortex_ui_settings"
    private const val KEY = "file_auto_accept"

    private var prefs: SharedPreferences? = null
    private val _enabled = MutableStateFlow(false)

    /** Observable: whether incoming files skip the consent prompt. */
    val enabled: StateFlow<Boolean> = _enabled.asStateFlow()

    /** Load persisted state. Idempotent — safe to call from the UI and the service. */
    @Synchronized
    fun init(context: Context) {
        if (prefs != null) return
        val p = context.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        prefs = p
        _enabled.value = p.getBoolean(KEY, false)
    }

    fun isEnabled(): Boolean = _enabled.value

    fun setEnabled(enabled: Boolean) {
        _enabled.value = enabled
        prefs?.edit()?.putBoolean(KEY, enabled)?.apply()
    }
}
