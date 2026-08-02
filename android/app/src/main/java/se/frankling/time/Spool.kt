package se.frankling.time

import org.json.JSONObject
import java.io.File

/**
 * Minutes derived from the system event log but not yet accepted by the server.
 *
 * The client used to have no local buffer at all, on the reasoning that
 * `UsageStatsManager` *is* the write-ahead log: nothing was uploaded, nothing
 * was lost, the events were still there next run. That holds exactly as long as
 * the events are — about a week. Past that the window is clamped to what the
 * system still retains and everything older simply stops existing, silently,
 * with no error and no gap on the chart to notice. A fortnight abroad, or any
 * stretch where the phone has internet but never the LAN or the VPN, is enough:
 * the sync runs every half hour the whole time, gets a 403, and keeps not
 * saving anything.
 *
 * So the derived minutes are written here as soon as they are read, and the
 * watermark advances on *that* rather than on the upload. What the server has
 * not confirmed stays on disk until it does. The frames hold no screenshot —
 * Android cannot take one — so this is a few hundred bytes a minute.
 *
 * One JSON array rewritten whole rather than an append log. At this size (a
 * month of real phone use is well under a megabyte) a rewrite is cheaper than
 * the code needed to compact an append log would be, it makes deduplicating the
 * deliberate overlap between runs a map insert, and a torn write can only
 * damage the temporary file.
 */
class Spool(private val file: File) {

    /**
     * How much history is worth keeping if the server is never reachable.
     *
     * Deliberately far past the system's ~7 days: escaping that limit is the
     * only reason this file exists, so a cap anywhere near it would rebuild the
     * cliff one level up. A real day is a few hundred phone minutes, so 60,000
     * is months of ordinary use, and the age bound is what normally applies.
     */
    private val maxFrames = 60_000
    private val maxAgeSecs = 30L * 24 * 3600

    /** Everything still owed to the server, oldest first. */
    fun read(): List<JSONObject> {
        if (!file.exists()) return emptyList()
        return try {
            val text = file.readText()
            val arr = org.json.JSONArray(text)
            (0 until arr.length()).map { arr.getJSONObject(it) }
        } catch (_: Exception) {
            // A file that will not parse will not parse next run either, and
            // wedging every future sync behind it is worse than losing it.
            file.delete()
            emptyList()
        }
    }

    /**
     * Fold freshly derived minutes in and persist.
     *
     * Runs overlap on purpose, so the same minute arrives repeatedly; last
     * write wins, because the later read of a minute saw more of it. Returns
     * the full backlog, oldest first — which is the order it has to be sent in,
     * since the server classifies each minute with the previous one as context.
     */
    fun merge(fresh: List<JSONObject>, nowSecs: Long): List<JSONObject> {
        val byTs = sortedMapOf<Long, JSONObject>()
        for (f in read()) byTs[f.optLong("ts")] = f
        for (f in fresh) byTs[f.optLong("ts")] = f

        val floor = nowSecs - maxAgeSecs
        val dropped = byTs.keys.filter { it < floor }.toList()
        for (ts in dropped) byTs.remove(ts)
        // Oldest first, so what survives is the part still worth uploading.
        while (byTs.size > maxFrames) byTs.remove(byTs.firstKey())

        val out = byTs.values.toList()
        write(out)
        return out
    }

    /** Forget minutes the server has answered 2xx for. */
    fun ack(sent: Collection<Long>): List<JSONObject> {
        val gone = sent.toSet()
        val out = read().filter { it.optLong("ts") !in gone }
        write(out)
        return out
    }

    private fun write(frames: List<JSONObject>) {
        val tmp = File(file.parentFile, file.name + ".tmp")
        // Renamed into place, so a kill mid-write leaves the previous backlog
        // intact rather than a truncated one.
        tmp.writeText(org.json.JSONArray(frames).toString())
        if (!tmp.renameTo(file)) {
            file.delete()
            tmp.renameTo(file)
        }
    }
}
