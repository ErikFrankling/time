package se.frankling.time

import org.json.JSONArray
import java.io.File

/**
 * Events read from the phone but not yet accepted by the server.
 *
 * Android's usage log forgets after about a week and Health Connect reads
 * are not free to repeat, so whatever has been read goes here first and the
 * read watermarks advance on *that*, not on the upload. What the server has
 * not acknowledged stays on disk until it does: a fortnight off the VPN loses
 * nothing up to the caps below.
 *
 * Keyed by event id. Reads deliberately overlap, and an event re-read with
 * the same id replaces the earlier copy instead of duplicating it. One JSON
 * array rewritten whole and renamed into place: at this size a rewrite is
 * cheaper than the code to compact an append log, and a torn write can only
 * damage the temporary file.
 */
class Spool(private val file: File) {

    private val maxEvents = 60_000
    private val maxAgeMs = 30L * 24 * 3600_000

    fun read(): List<Envelope> {
        if (!file.exists()) return emptyList()
        return try {
            val arr = JSONArray(file.readText())
            (0 until arr.length()).map { Envelope.fromJson(arr.getJSONObject(it)) }
        } catch (_: Exception) {
            // A file that will not parse never will; wedging every future
            // sync behind it is worse than losing it.
            file.delete()
            emptyList()
        }
    }

    /** Fold new events in; returns the whole backlog, oldest first. */
    fun merge(fresh: List<Envelope>, nowMs: Long): List<Envelope> {
        val byId = LinkedHashMap<String, Envelope>()
        for (e in read()) byId[e.id] = e
        for (e in fresh) byId[e.id] = e
        val kept = byId.values
            .filter { it.endMs >= nowMs - maxAgeMs }
            .sortedWith(compareBy({ it.startMs }, { it.id }))
            .takeLast(maxEvents)
        write(kept)
        return kept
    }

    /** Forget events the server has answered 2xx for. */
    fun ack(ids: Collection<String>): List<Envelope> {
        val gone = ids.toSet()
        val out = read().filter { it.id !in gone }
        write(out)
        return out
    }

    fun size(): Int = read().size

    private fun write(events: List<Envelope>) {
        val tmp = File(file.parentFile, file.name + ".tmp")
        tmp.writeText(JSONArray(events.map { it.toJson() }).toString())
        if (!tmp.renameTo(file)) {
            file.delete()
            tmp.renameTo(file)
        }
    }
}
