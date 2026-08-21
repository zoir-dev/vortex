package com.vortex.a3.service

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test

/**
 * The outgoing-file-offer policy. A phone→laptop share is announced over BLE
 * and pulled over LAN, and every step can fail silently — including a notify
 * the local stack accepts and the link then drops. This decides when to send,
 * when to re-announce, and when to admit defeat; getting it wrong is
 * user-visible both ways (a batch still in the queue reported as lost, or a
 * share that went nowhere staying silent for ever).
 */
class OfferVerdictTest {

    /** [lastSentAtMs] doubles as "has it ever gone out"; [deadlineFromMs] is
     *  the give-up clock, which callers slide on batch progress. */
    private fun offer(
        attempts: Int = 0,
        lastSentAtMs: Long = 0L,
        deadlineFromMs: Long = lastSentAtMs,
    ) = PendingOffer("tok", "file.bin", ByteArray(0)).also {
        it.attempts = attempts
        it.lastSentAtMs = lastSentAtMs
        it.deadlineFromMs = deadlineFromMs
    }

    @Test
    fun `an offer that never went out is retried until its budget runs out`() {
        assertEquals(OfferVerdict.DELIVER, offerVerdict(offer(attempts = 0), 0L))
        assertEquals(
            OfferVerdict.DELIVER,
            offerVerdict(offer(attempts = OFFER_MAX_ATTEMPTS - 1), 0L),
        )
        assertEquals(
            OfferVerdict.GIVE_UP,
            offerVerdict(offer(attempts = OFFER_MAX_ATTEMPTS), 0L),
        )
    }

    @Test
    fun `a sent offer waits for the pull`() {
        val sent = offer(attempts = 1, lastSentAtMs = 1_000L)
        assertEquals(OfferVerdict.WAIT, offerVerdict(sent, 1_000L))
        assertEquals(OfferVerdict.WAIT, offerVerdict(sent, 1_000L + OFFER_RESEND_MS - 1))
    }

    @Test
    fun `a sent offer nothing came to collect is re-announced`() {
        // The dropped-notify case: the local stack took the frame, the link
        // lost it, so the laptop never knew to pull. Waiting can't fix that.
        val sent = offer(attempts = 1, lastSentAtMs = 1_000L)
        assertEquals(OfferVerdict.DELIVER, offerVerdict(sent, 1_000L + OFFER_RESEND_MS))
    }

    @Test
    fun `re-announcing does not postpone giving up`() {
        // Sent repeatedly (so `lastSentAtMs` keeps moving) while the give-up
        // clock stays put: the deadline must still land.
        val stubborn = offer(attempts = 5, lastSentAtMs = 119_000L, deadlineFromMs = 0L)
        assertEquals(OfferVerdict.GIVE_UP, offerVerdict(stubborn, OFFER_PULL_GRACE_MS))
    }

    @Test
    fun `progress in the batch slides the deadline and buys more time`() {
        // What `noteFileServed` does to the rest of the batch: a fetch anywhere
        // proves the link works, so the others aren't declared lost while they
        // wait their turn (the laptop pulls one file per heartbeat round).
        val queued = offer(attempts = 1, lastSentAtMs = 1_000L, deadlineFromMs = 1_000L)
        val past = 1_000L + OFFER_PULL_GRACE_MS
        assertEquals(OfferVerdict.GIVE_UP, offerVerdict(queued, past))
        queued.deadlineFromMs = past
        queued.lastSentAtMs = past
        assertEquals(OfferVerdict.WAIT, offerVerdict(queued, past))
    }

    @Test
    fun `send attempts stop mattering once it has gone out`() {
        val sent = offer(attempts = OFFER_MAX_ATTEMPTS, lastSentAtMs = 500L)
        assertEquals(OfferVerdict.WAIT, offerVerdict(sent, 500L))
    }

    @Test
    fun `the give-up reason distinguishes never-sent from never-fetched`() {
        assertEquals(
            "the offer never reached the laptop in 20 attempts",
            giveUpReason(offer(attempts = 20)),
        )
        assertTrue(
            giveUpReason(offer(attempts = 1, lastSentAtMs = 5L)).contains("never fetched it"),
        )
    }
}

/**
 * Spotting an offer the BLE link dropped in flight. The phone cannot see the
 * loss directly — its own stack accepted the notify — so the only evidence is
 * the laptop fetching a LATER offer while an earlier one is still outstanding,
 * its pull queue being FIFO in arrival order.
 */
class PresumedDroppedTest {

    private fun offer(seq: Long, lastSentAtMs: Long) =
        PendingOffer("tok$seq", "file$seq.bin", ByteArray(0), seq = seq).also {
            it.lastSentAtMs = lastSentAtMs
            it.deadlineFromMs = lastSentAtMs
        }

    @Test
    fun `an earlier offer skipped over was dropped in flight`() {
        val first = offer(seq = 1, lastSentAtMs = 100L)
        val third = offer(seq = 3, lastSentAtMs = 100L)
        // The laptop fetched #2, so #1 would have come first had it arrived.
        val dropped = offersPresumedDropped(listOf(first, third), fetchedSeq = 2)
        assertEquals(listOf(first), dropped)
    }

    @Test
    fun `a later offer is simply waiting its turn`() {
        // One file per heartbeat round: #3 outstanding after #2 was fetched is
        // the normal case and must NOT be re-announced.
        val third = offer(seq = 3, lastSentAtMs = 100L)
        assertEquals(emptyList<PendingOffer>(), offersPresumedDropped(listOf(third), 2))
    }

    @Test
    fun `an offer that never went out is left to the send retry`() {
        // Nothing to infer: it isn't missing, it hasn't been sent. The delivery
        // budget owns this one.
        val unsent = offer(seq = 1, lastSentAtMs = 0L)
        assertEquals(emptyList<PendingOffer>(), offersPresumedDropped(listOf(unsent), 2))
    }
}
