package com.vortex.a3.core.clipboard

import android.app.AppOpsManager
import android.content.Context
import android.os.Process

/**
 * Whether the `READ_CLIPBOARD` AppOp is set to ALLOW for this app.
 *
 * **This is NOT sufficient for background clipboard reads, despite the name.**
 * Measured on Android 16, LineageOS-based ROM (PJZ110, 2026-09-16): with the op
 * set to ALLOW, the app doze-whitelisted, unrestricted in the background and
 * running a foreground service, [ClipboardListener]'s change callback was never
 * delivered once and the op was not noted for 3.8 days of ordinary copying.
 *
 * Near-AOSP is the important part: this is the stock platform gate, not an OEM
 * clampdown, so it is what every reasonably current Android will do and not
 * something a different ROM or a vendor workaround gets around.
 *
 * The platform gates clipboard access on TWO conditions, not one: the caller
 * must first qualify as allowed — by holding
 * `android.permission.READ_CLIPBOARD_IN_BACKGROUND`, by being the default IME,
 * or by owning the focused window — and only then is this AppOp consulted. A
 * sideloaded Vortex is none of the three, and that permission is
 * `signature|role`, so it cannot be granted by any route available to us
 * (`pm grant` refuses it with "managed by role").
 *
 * So automatic background capture is not reachable by setting the op, and the
 * ADB incantation this doc used to recommend does nothing on its own. The
 * user-triggered paths are the real mechanism: the Quick Settings tile and the
 * share sheet both run [ClipboardQuickSendActivity] in the FOREGROUND, which
 * satisfies the focused-window condition and is why they work.
 *
 * Kept as a UX hint, and because the op is still the necessary half of the
 * pair for any caller that does qualify.
 */
object ClipboardAccess {
    /** True if the AppOp is set to ALLOW (background reads work → auto sync). */
    fun isBackgroundReadGranted(context: Context): Boolean = try {
        val ops = context.getSystemService(Context.APP_OPS_SERVICE) as AppOpsManager
        val mode = ops.unsafeCheckOpNoThrow(
            "android:read_clipboard",
            Process.myUid(),
            context.packageName,
        )
        mode == AppOpsManager.MODE_ALLOWED
    } catch (_: Exception) {
        false
    }
}
