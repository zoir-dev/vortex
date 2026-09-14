package com.vortex.a3.service

import com.vortex.a3.core.ble.FrameType
import com.vortex.a3.core.fs.FsHandles
import com.vortex.a3.core.fs.FsRoots
import com.vortex.a3.core.fs.FsServer
import com.vortex.a3.core.fs.FsCode
import com.vortex.a3.core.fs.FsErr
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch

/**
 * Wires filesystem serving (FS_REQ 0x50 → FS_META / FS_DATA / FS_ERR) into the
 * BLE stack: the phone half of `docs/design/file-browsing.md`, answering the
 * laptop's ranged reads against the folders the user has shared.
 *
 * Serving is READ-ONLY in v1. Writes are refused explicitly rather than
 * dropped — see [FsServer].
 *
 * Extension fn on [VortexStack]; call once after the GATT server is up.
 */
internal fun VortexStack.startFsServer() {
    val roots = FsRoots(ctx)
    val handles = FsHandles()
    val server = FsServer(ctx, roots, handles)
    fsHandles = handles
    // A share-sheet file the laptop just finished reading. Same completion
    // signal the old bulk-sync path got from writing the whole blob onto the
    // socket: advances the batch's progress and releases the next queued file.
    server.onShareDelivered = { token -> noteFileServed(token) }

    // Wi-Fi path. The laptop prefers it and falls back to BLE, so BOTH
    // transports serve from this one server and one handle table: a handle is
    // minted by OPEN and used by later READs, and a fallback between the two
    // would otherwise answer BADF halfway through a file.
    //
    // Runs on the LanServer connection's own thread rather than being hopped
    // onto Dispatchers.IO: that thread exists to serialise this socket's
    // frames, and the reply must be written before the next op is read.
    val serve: (Byte, ByteArray) -> Pair<Byte, ByteArray> = { op, payload ->
        val srv = try {
            server.serve(op, payload)
        } catch (e: Exception) {
            FsServer.Served.Err(FsErr(0, FsCode.IO, e.message ?: "serve failed"))
        }
        when (srv) {
            is FsServer.Served.Meta -> FrameType.FS_META to srv.reply.toJsonBytes()
            is FsServer.Served.Data -> FrameType.FS_DATA to srv.bytes
            is FsServer.Served.Err -> FrameType.FS_ERR to srv.err.toJsonBytes()
        }
    }
    fsServeFn = serve
    // Null on first start (BLE comes up before the LAN server, which installs
    // it itself); non-null on a BLE restart, which replaces the server and
    // handle table the LAN side was still pointing at.
    lanServer?.fsServe = serve

    // The other direction: browsing the LAPTOP's files from this phone. Same
    // ops, same frames — the protocol is symmetric — so the client needs only a
    // way to send and a way to be handed replies.
    com.vortex.a3.core.fs.FsClient.sender = { op, payload ->
        // Wi-Fi first, exactly as the laptop does for the same frames: a 48 KiB
        // read is one TCP frame against ~96 paced BLE fragments.
        //
        // The first request of a browse still goes over BLE, and cannot not:
        // the phone has no way to dial the laptop, which runs no listener. What
        // it can do is answer on the session the laptop opens to deliver its
        // reply — that socket is bidirectional and the laptop serves whatever
        // arrives on it — so BLE carries the opening request and Wi-Fi carries
        // the rest, including every ranged read of a download.
        lanServer?.fsSend(op, payload) == true || run {
            val peer = activePeerPub ?: peerStore.list().firstOrNull()?.peerStaticPub
            peer != null && gattServer?.sendFsRequest(peer, op, payload) == true
        }
    }
    gattServer?.onFsReply = { _, type, payload ->
        com.vortex.a3.core.fs.FsClient.onReply(type, payload)
    }
    // The same replies can arrive over Wi-Fi instead: the laptop's client picks
    // its transport per send, so the one that carried our request is not
    // necessarily the one that answers it.
    lanServer?.onFsReply = { type, payload ->
        com.vortex.a3.core.fs.FsClient.onReply(type, payload)
    }
    fsReplyFn = { type, payload -> com.vortex.a3.core.fs.FsClient.onReply(type, payload) }

    gattServer?.onFsRequest = { peerPub, op, payload ->
        // Off the GATT callback thread, always. A document provider can stall
        // for seconds — a cloud-backed one indefinitely — and blocking here
        // would stall every other frame on the link behind one slow folder,
        // including the audio-switch path that shares this characteristic.
        scope.launch(Dispatchers.IO) {
            val srv = try {
                server.serve(op, payload)
            } catch (e: Exception) {
                // Never let an unexpected provider exception become silence:
                // the laptop is blocked on this request id and would wait out
                // its timeout instead of showing an error.
                FsServer.Served.Err(FsErr(0, FsCode.IO, e.message ?: "serve failed"))
            }
            val (type, bytes) = when (srv) {
                is FsServer.Served.Meta -> FrameType.FS_META to srv.reply.toJsonBytes()
                is FsServer.Served.Data -> FrameType.FS_DATA to srv.bytes
                is FsServer.Served.Err -> FrameType.FS_ERR to srv.err.toJsonBytes()
            }
            gattServer?.sendFsReply(peerPub, type, bytes)
        }
    }
}

/** Drop every open handle for a link that has gone. Handles cannot outlive the
 *  session that owns them: the ids are only meaningful to that peer, and an
 *  abandoned descriptor is a leak the idle sweep would take five minutes to
 *  notice. */
internal fun VortexStack.stopFsServer() {
    fsHandles?.clear()
    // Nothing in flight can be answered once the link is gone; fail the waiters
    // now rather than leaving the UI parked until each one times out.
    com.vortex.a3.core.fs.FsClient.reset()
}
