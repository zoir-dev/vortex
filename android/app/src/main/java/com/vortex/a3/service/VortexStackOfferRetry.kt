package com.vortex.a3.service

import android.util.Log
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull

/**
 * Delivery tracking for phone→laptop FILE offers — the half of instant-share
 * that used to fail in silence.
 *
 * A shared file is stashed locally and announced to the laptop as a small OFFER
 * frame over BLE; the laptop then PULLS the bytes over LAN. Both steps can fail
 * with nothing to show for it:
 *
 *  - the OFFER is a fire-and-forget BLE notify. With no live session (the phone
 *    app restarted, the user walked out of range, the laptop is re-handshaking)
 *    it simply doesn't go out — and the send result was discarded, so the log
 *    claimed success and the user saw a "Sending…" toast for a file the laptop
 *    never heard about;
 *  - the offer can land while the LAN pull never happens (laptop asleep, no
 *    route, consent declined), which is equally invisible from the phone.
 *
 * So every offer is tracked until the laptop has actually FETCHED it: retried
 * while it can't be delivered, watched for a pull once it has been, and
 * surfaced as a toast when it ends up nowhere. The stashed blob is untouched
 * either way — [com.vortex.a3.core.clipboard.ClipboardBlobStore] keeps it
 * addressable, so a later re-share of the same file is free.
 */

/** One outgoing file offer, tracked until the laptop fetches it. */
internal class PendingOffer(
    val token: String,
    val name: String,
    val offer: ByteArray,
    /** Order this offer was queued in. The laptop pulls its queue FIFO, so
     *  comparing sequences tells us something its own reports can't — see
     *  [offersPresumedDropped]. */
    val seq: Long = 0L,
) {
    /** BLE send attempts made so far. Only bounds the never-delivered case. */
    var attempts: Int = 0

    /** `elapsedRealtime()` when the OFFER frame was last handed to the BLE
     *  stack successfully; 0 while it has never gone out. Note what this can
     *  NOT tell us: `sealAndNotify` returning true means the LOCAL stack
     *  queued the notify, not that the laptop received it — a notify can still
     *  be dropped in flight (observed: `resynced past dropped BLE frame(s)`
     *  swallowing one offer out of three). Hence [OFFER_RESEND_MS]: an offer
     *  that stays unfetched gets re-announced rather than assumed delivered. */
    var lastSentAtMs: Long = 0L

    /** The clock the give-up deadline runs from: first successful send, then
     *  slid forward by progress anywhere in the batch. Separate from
     *  [lastSentAtMs] on purpose — re-announcing must not postpone giving up
     *  for ever. */
    var deadlineFromMs: Long = 0L
}

/**
 * Offer [name] to the laptop and keep at it until the bytes are fetched: the
 * send is retried while it can't go out, RE-announced while it has gone out but
 * nothing came to collect it, and reported as lost if neither ever happens.
 */
internal suspend fun VortexStack.offerFileToLaptop(token: String, name: String, offer: ByteArray) {
    val pending = PendingOffer(token, name, offer, seq = ++offerSeq)
    pendingOffers[token] = pending
    // First attempt inline: the common case is a live link, where queueing and
    // waiting out a tick would add seconds to an otherwise instant share.
    if (!tryDeliverOffer(pending)) {
        Log.w(
            VortexStack.TAG,
            "file offer for '$name' couldn't go out (BLE link down?); retrying",
        )
        // Say so ONCE per outage, not once per file: the wait that follows is
        // the whole complaint. A share can otherwise sit silent for a minute
        // (observed: 29 s for the BLE link to come back, then another 30 s for
        // a dropped offer to be re-announced) with nothing on screen since the
        // share sheet's "Sending…".
        if (!offerUnreachableToasted) {
            offerUnreachableToasted = true
            toastOffer("Laptop unreachable — keeping the file(s) queued")
        }
    }
    startOfferWatchdog()
}

/** Push [pending]'s OFFER frame to every trusted peer. Returns true when the
 *  BLE stack took it, and marks it sent + warms the LAN path. */
private suspend fun VortexStack.tryDeliverOffer(pending: PendingOffer): Boolean {
    pending.attempts++
    val server = gattServer ?: return false
    var delivered = false
    for (peer in peerStore.list()) {
        if (server.sendClipboardImageOfferEncrypted(peer.peerStaticPub, pending.offer)) {
            delivered = true
        }
    }
    if (!delivered) return false
    val now = android.os.SystemClock.elapsedRealtime()
    pending.lastSentAtMs = now
    if (pending.deadlineFromMs == 0L) pending.deadlineFromMs = now
    // Link is back — arm the "unreachable" notice again for the next outage.
    offerUnreachableToasted = false
    Log.i(
        VortexStack.TAG,
        "file offer for '${pending.name}' sent (attempt ${pending.attempts})",
    )
    // PACE the burst, as every other bulk BLE path here does. A share of N
    // files fires N offers back-to-back, and unpaced they overrun the notify
    // queue: three offers 4-9 ms apart cost one of them outright (the laptop
    // logged `resynced past dropped BLE frame(s) dropped=1` and queued 2 of 3
    // files) while the phone believed all three had landed.
    kotlinx.coroutines.delay(OFFER_PACING_MS)
    scheduleLanWarm()
    return true
}

/**
 * Warm the LAN path for the imminent pull, once the offer burst has settled.
 *
 * The laptop has to REACH us, once per queued file, and two things stop it:
 * while BLE is up we release the multicast lock (so mDNS goes unanswered and
 * its cached IP — stale after any DHCP renew — is its only guess), and the
 * Wi-Fi radio parks between its rounds. So hold the radio + mDNS open and
 * re-announce, and push our AppState over BLE too: it carries our live
 * `wifi_ip`, which repoints the laptop's cache with no mDNS involved at all.
 *
 * DEFERRED and debounced, because doing this inline per offer put an NSD
 * re-announce and a STATE notify between offer 1 and offer 2 — and that notify
 * is what cost us offer 1 (observed: sent at .474, `LAN hot` at .538, offer 2
 * at .549, and the laptop only ever saw offers 2 and 3). Pacing the offers
 * against each other is pointless if something else cuts in line.
 */
private fun VortexStack.scheduleLanWarm() {
    lanWarmJob?.cancel()
    lanWarmJob = scope.launch {
        kotlinx.coroutines.delay(LAN_WARM_SETTLE_MS)
        // Re-announcing costs an NSD round-trip, so only the first offer of a
        // batch does it; later ones just extend the window.
        if (lanServer?.keepLanHot() == true) {
            // Skip the redundant push right after a reconnect: the BLE
            // re-subscribe handler has already sent one, and a second would be
            // one more frame competing with the offers we just queued.
            if (android.os.SystemClock.elapsedRealtime() - lastBleStatePushAtMs
                >= STATE_PUSH_DEDUP_MS
            ) {
                pushStateViaBle()
            }
        }
    }
}

/** The laptop served itself the blob for [token] over LAN — the offer did its
 *  job. Wired to `LanServer.onFileServed`. */
internal fun VortexStack.noteFileServed(token: String) {
    val done = pendingOffers.remove(token) ?: return
    Log.i(VortexStack.TAG, "file '${done.name}' fetched by the laptop")
    // The one unambiguous "it worked" moment on this device: the laptop has the
    // bytes. Per file rather than per batch, so a slow batch shows progress as
    // it goes instead of one summary at the end.
    toastOffer("File sent: ${done.name}")
    // SLIDING deadline, like the daemon's bulk-sync idle budget: the laptop
    // pulls one file per heartbeat round, so a big batch's last offer can
    // legitimately wait many minutes for its turn. A fetch anywhere in the
    // batch proves the link is working — restart the clock on the rest rather
    // than reporting a failure for files that are simply still queued.
    val now = android.os.SystemClock.elapsedRealtime()
    for (still in pendingOffers.values) {
        if (still.deadlineFromMs != 0L) still.deadlineFromMs = now
    }
    // Anything the laptop skipped over never reached it: re-announce at once
    // rather than waiting out the resend timer. Clearing `lastSentAtMs` is the
    // honest record — as far as the laptop is concerned this offer never
    // happened — and puts it back in the DELIVER path on the next tick.
    val dropped = offersPresumedDropped(pendingOffers.values, done.seq)
    for (lost in dropped) {
        lost.lastSentAtMs = 0L
        // Reset the delivery budget too. It exists to bound a link that won't
        // carry the offer at all, and we have just PROVED this one carries —
        // the laptop fetched a file. Without this, an offer dropped after a
        // long outage (say 19 of 20 attempts spent) would be declared lost on
        // the spot instead of getting the one re-announce it needs.
        lost.attempts = 0
        Log.w(
            VortexStack.TAG,
            "offer for '${lost.name}' was dropped in flight (the laptop fetched a " +
                "later one first); re-announcing now",
        )
    }
    if (dropped.isNotEmpty()) kickOfferRetry()
}

/**
 * Offers we can PROVE the laptop never received, given that it just fetched
 * [fetchedSeq]. Its pull queue is FIFO in offer-arrival order, so anything
 * queued before the file it just took would have been fetched first — an
 * earlier offer still sitting here was dropped in flight, not merely waiting
 * its turn.
 *
 * This is the only way the phone can tell: the BLE notify was accepted locally,
 * so nothing on this side reports the loss. Without it, the file waits out
 * [OFFER_RESEND_MS] with the user watching nothing happen (observed: 2 of 3
 * files arrived in 3 s, the third took another 30 s for no reason but the
 * timer).
 */
internal fun offersPresumedDropped(
    pending: Collection<PendingOffer>,
    fetchedSeq: Long,
): List<PendingOffer> = pending.filter { it.lastSentAtMs != 0L && it.seq < fetchedSeq }

/** Wake the watchdog now instead of at its next tick — called when the BLE link
 *  comes back, which is exactly when a queued offer can finally go out. */
internal fun VortexStack.kickOfferRetry() {
    if (pendingOffers.isEmpty()) return
    offerRetryKick.trySend(Unit)
}

/** Start the watchdog if it isn't already running. Idempotent: one loop drains
 *  the whole map, and it exits when the map empties. */
private fun VortexStack.startOfferWatchdog() {
    if (offerRetryJob?.isActive == true) return
    offerRetryJob = scope.launch { offerWatchdog() }
}

/**
 * Retry undelivered offers, time out delivered-but-unfetched ones, and toast
 * whatever ends up nowhere. Runs only while offers are outstanding.
 */
private suspend fun VortexStack.offerWatchdog() {
    while (pendingOffers.isNotEmpty()) {
        withTimeoutOrNull(OFFER_RETRY_TICK_MS) { offerRetryKick.receive() }
        val now = android.os.SystemClock.elapsedRealtime()
        val lost = mutableListOf<String>()
        // Snapshot the values: `tryDeliverOffer` and `noteFileServed` both
        // mutate the map while we walk it.
        for (pending in pendingOffers.values.toList()) {
            // Send first when it's due — `tryDeliverOffer` bumps `attempts`, so
            // the second verdict sees the budget this attempt just consumed.
            if (offerVerdict(pending, now) == OfferVerdict.DELIVER && tryDeliverOffer(pending)) {
                continue
            }
            if (offerVerdict(pending, now) != OfferVerdict.GIVE_UP) continue
            pendingOffers.remove(pending.token)
            lost += pending.name
            Log.w(VortexStack.TAG, "giving up on '${pending.name}': ${giveUpReason(pending)}")
        }
        if (lost.isNotEmpty()) toastOffersLost(lost)
    }
}

/** Tell the user, on this phone, that [lost] never made it. The log carries the
 *  reason; this only has to stop the share looking like it worked. */
private fun VortexStack.toastOffersLost(lost: List<String>) {
    toastOffer(
        if (lost.size == 1) {
            "Laptop didn't get '${lost.first()}'"
        } else {
            "Laptop didn't get ${lost.size} files"
        },
    )
}

/** Show [msg] on this phone. Transfer feedback only — the share leaves the
 *  device and nothing else here reports on it. */
private fun VortexStack.toastOffer(msg: String) {
    android.os.Handler(android.os.Looper.getMainLooper()).post {
        try {
            // English only, like the cast-failure toast: `ui/Strings.kt`'s
            // `str()` is @Composable and unavailable from a service.
            android.widget.Toast.makeText(ctx, msg, android.widget.Toast.LENGTH_LONG).show()
        } catch (t: Throwable) {
            // Best-effort (some ROMs suppress background toasts); the log line
            // above is the durable record.
            Log.w(VortexStack.TAG, "offer-failure toast suppressed: ${t.message}")
        }
    }
}

/** What the watchdog should do with an offer right now. */
internal enum class OfferVerdict {
    /** Not delivered yet and still within budget — (re)send the OFFER frame. */
    DELIVER,

    /** Delivered; the laptop still has time to fetch the blob. */
    WAIT,

    /** Out of delivery attempts, or out of patience waiting for the pull. */
    GIVE_UP,
}

/**
 * The whole give-up policy, kept pure so it can be tested without a BLE stack
 * or a clock: an offer that was never delivered gets [OFFER_MAX_ATTEMPTS]
 * tries, and one that was gets [OFFER_PULL_GRACE_MS] from the batch's last
 * progress to actually be fetched.
 */
internal fun offerVerdict(pending: PendingOffer, nowMs: Long): OfferVerdict = when {
    pending.lastSentAtMs == 0L ->
        if (pending.attempts >= OFFER_MAX_ATTEMPTS) OfferVerdict.GIVE_UP else OfferVerdict.DELIVER
    nowMs - pending.deadlineFromMs >= OFFER_PULL_GRACE_MS -> OfferVerdict.GIVE_UP
    // Sent, still unfetched: the notify may have been dropped in flight, which
    // no amount of waiting recovers. Re-announce — the laptop dedups by token,
    // so a duplicate that DID arrive costs nothing.
    nowMs - pending.lastSentAtMs >= OFFER_RESEND_MS -> OfferVerdict.DELIVER
    else -> OfferVerdict.WAIT
}

/** Why we stopped tracking [pending] — the log's half of the toast. */
internal fun giveUpReason(pending: PendingOffer): String =
    if (pending.lastSentAtMs == 0L) {
        "the offer never reached the laptop in ${pending.attempts} attempts"
    } else {
        "the laptop accepted the offer but never fetched it " +
            "(asleep, no LAN route, or declined)"
    }

/** How often the watchdog re-tries delivery and re-checks the pull deadline. */
internal const val OFFER_RETRY_TICK_MS = 3_000L

/** Delivery attempts before giving up — ~1 min at [OFFER_RETRY_TICK_MS], which
 *  covers a BLE reconnect (observed: up to ~1m45s with an adapter power-cycle
 *  on the laptop, so a walk-away is still reported rather than waited out). */
internal const val OFFER_MAX_ATTEMPTS = 20

/** How long the laptop gets to actually FETCH a delivered offer, measured from
 *  the last progress anywhere in the batch (see [noteFileServed]). Generous: it
 *  may have to re-find us on the LAN, and with consent prompts enabled a human
 *  has to click Accept (that banner itself times out at 45 s). */
internal const val OFFER_PULL_GRACE_MS = 120_000L

/** Gap between consecutive offer notifies. Wider than the 12-20 ms the chunk
 *  streams use, because offers are few (one per shared file, so ~0.6 s even for
 *  a ten-file share) and they go out at the worst possible moment: a BLE
 *  reconnect, where the state push, notes sync, icon re-send, live activities
 *  and companion mirrors all queue at once. The drop that prompted this had
 *  offers only 4-9 ms apart, so "no pacing at all" was well inside the danger
 *  zone; the re-announce below is what covers the rest. */
internal const val OFFER_PACING_MS = 60L

/** How long a sent-but-unfetched offer waits before being re-announced. Covers
 *  a notify dropped in flight, which the phone cannot otherwise detect. Long
 *  enough that a laptop working through a batch (one file per heartbeat round)
 *  isn't pestered mid-pull. */
internal const val OFFER_RESEND_MS = 30_000L

/** How long after the last offer went out to warm the LAN path. Long enough
 *  for the offer burst to drain the notify queue first. */
internal const val LAN_WARM_SETTLE_MS = 250L

/** A BLE AppState push within this window of the last one is redundant — the
 *  reconnect handler already sent it, and it would only crowd the offers. */
internal const val STATE_PUSH_DEDUP_MS = 5_000L

/** Conflated so a burst of BLE re-subscribes collapses into one wake-up. */
internal fun newOfferRetryKick(): Channel<Unit> = Channel(Channel.CONFLATED)
