package dev.leptoskit.quant

import android.content.Intent
import dev.leptoskit.bridge.NativePlugin
import dev.leptoskit.bridge.PluginException
import java.nio.ByteBuffer
import java.nio.charset.StandardCharsets
import kotlinx.coroutines.CompletableDeferred

/**
 * Holds one inbound `quant://link` for the screen to review.
 *
 * The manifest delivers VIEW intents; this plugin validates the scheme and
 * host, keeps the latest link, and lets the guest take it. It never pairs,
 * replaces a vault, or changes collection state. Receiving a link only asks
 * the screen to show the Connect card.
 */
class QuantLinkPlugin : NativePlugin {
    override val name = NAME
    override val methods = intArrayOf(NEXT)

    override suspend fun invoke(
        method: Int,
        payload: ByteBuffer,
    ): ByteArray {
        if (method != NEXT) throw PluginException(PluginException.UNAVAILABLE)
        if (payload.hasRemaining()) throw PluginException(PluginException.INVALID_ARGUMENT)
        val link = inbox.take() ?: throw PluginException(PluginException.CANCELLED)
        return link.toByteArray(StandardCharsets.UTF_8)
    }

    companion object {
        // Must match quant_model::phone::{LINK_PLUGIN, LINK_NEXT}.
        const val NAME = "quant-link"

        private const val NEXT = 1
        private const val SCHEME = "quant"
        private const val HOST = "link"
        private const val MAX_BYTES = 4096
        private val inbox = LinkInbox()

        /**
         * Notes an inbound link and consumes its URI so an Activity recreation
         * cannot replay it. A malformed link is dropped without logging it.
         */
        @JvmStatic
        fun noteIntentIfPresent(intent: Intent?) {
            if (intent == null || intent.action != Intent.ACTION_VIEW) return
            val data = intent.data ?: return
            if (data.scheme != SCHEME || data.host != HOST) return
            val link = data.toString()
            if (link.toByteArray(StandardCharsets.UTF_8).size > MAX_BYTES) return
            inbox.offer(link)
            intent.data = null
        }
    }
}

/** One waiting link, or one guest waiting for the next one. */
private class LinkInbox {
    private val lock = Any()
    private var pending: String? = null
    private var waiter: CompletableDeferred<String?>? = null
    private var last: String? = null
    private var lastAtNanos = 0L

    /** Keeps the latest link, or wakes the one guest waiting for it. */
    fun offer(link: String) {
        val waiting =
            synchronized(lock) {
                if (pending == link) return
                val now = System.nanoTime()
                if (last == link && now - lastAtNanos < DUPLICATE_NANOS) return
                last = link
                lastAtNanos = now
                val waiting = waiter
                if (waiting != null) waiter = null else pending = link
                waiting
            }
        waiting?.complete(link)
    }

    /** Takes the pending link, waiting until the host offers one. */
    suspend fun take(): String? {
        val wait = CompletableDeferred<String?>()
        val ready =
            synchronized(lock) {
                val link = pending
                if (link != null) {
                    pending = null
                    link
                } else {
                    waiter = wait
                    null
                }
            }
        if (ready != null) return ready
        return try {
            wait.await()
        } finally {
            synchronized(lock) { if (waiter === wait) waiter = null }
        }
    }

    private companion object {
        /** Android and iOS can report one opening through two callbacks. */
        const val DUPLICATE_NANOS = 1_000_000_000L
    }
}
