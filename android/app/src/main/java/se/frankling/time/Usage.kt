package se.frankling.time

import android.app.AppOpsManager
import android.app.usage.UsageEvents
import android.app.usage.UsageStatsManager
import android.content.Context
import android.content.pm.PackageManager
import android.os.Process

/** A contiguous stretch with one app in the foreground. */
data class Session(val pkg: String, val start: Long, val end: Long)

/** One minute of phone activity, matching the server's Frame shape. */
data class MinuteFrame(
    val ts: Long,
    val window: String,
    val apps: List<String>,
    val idleSecs: Int,
    val note: String?,
)

object Usage {

    /**
     * Whether usage access has been granted.
     *
     * A plain `MODE_ALLOWED` check is wrong on some devices: when the op is
     * MODE_DEFAULT the answer comes from the manifest permission instead.
     */
    fun hasAccess(ctx: Context): Boolean {
        val ops = ctx.getSystemService(AppOpsManager::class.java) ?: return false
        val mode = ops.unsafeCheckOpNoThrow(
            AppOpsManager.OPSTR_GET_USAGE_STATS, Process.myUid(), ctx.packageName
        )
        return if (mode == AppOpsManager.MODE_DEFAULT) {
            ctx.checkCallingOrSelfPermission(
                android.Manifest.permission.PACKAGE_USAGE_STATS
            ) == PackageManager.PERMISSION_GRANTED
        } else {
            mode == AppOpsManager.MODE_ALLOWED
        }
    }

    /**
     * Rebuild foreground sessions from the system event log.
     *
     * A single-slot state machine, not RESUME/PAUSE pair matching. A late or
     * missing PAUSE otherwise leaves two apps looking simultaneously
     * foreground, which double-counts; closing the open slot whenever another
     * app resumes is what prevents that.
     *
     * Closing on SCREEN_NON_INTERACTIVE is the correctness bug everyone hits:
     * without it, whatever was open at bedtime appears to run all night.
     *
     * The trailing open session is deliberately not emitted — its duration is
     * not known yet, and emitting it would duplicate on the next run.
     */
    fun sessions(events: UsageEvents): List<Session> {
        val out = mutableListOf<Session>()
        var openPkg: String? = null
        var openStart = 0L

        fun close(end: Long) {
            val p = openPkg ?: return
            openPkg = null
            val d = end - openStart
            // Reject the absurd: sub-second blips and anything over four hours
            // are artefacts of a missed event, not real use.
            if (d in 1_000..(4 * 3600_000L)) out.add(Session(p, openStart, end))
        }

        val e = UsageEvents.Event()
        while (events.hasNextEvent()) {
            events.getNextEvent(e)
            when (e.eventType) {
                UsageEvents.Event.ACTIVITY_RESUMED -> {
                    // Re-resume of the same app keeps the earlier start.
                    if (openPkg == e.packageName) continue
                    close(e.timeStamp)
                    openPkg = e.packageName
                    openStart = e.timeStamp
                }
                UsageEvents.Event.ACTIVITY_PAUSED ->
                    if (openPkg == e.packageName) close(e.timeStamp)
                UsageEvents.Event.SCREEN_NON_INTERACTIVE,
                UsageEvents.Event.KEYGUARD_SHOWN,
                UsageEvents.Event.DEVICE_SHUTDOWN -> close(e.timeStamp)
            }
        }
        return out
    }

    /** Fan sessions out into one frame per minute. */
    fun frames(ctx: Context, sessions: List<Session>): List<MinuteFrame> {
        val pm = ctx.packageManager
        val labels = mutableMapOf<String, String>()
        fun label(pkg: String) = labels.getOrPut(pkg) {
            try {
                pm.getApplicationLabel(pm.getApplicationInfo(pkg, 0)).toString()
            } catch (_: Exception) {
                pkg
            }
        }

        // Several sessions can touch the same minute; the longest one wins it,
        // and every app that appeared is kept in `apps`.
        val byMinute = sortedMapOf<Long, MutableMap<String, Long>>()
        for (s in sessions) {
            var m = s.start / 60_000 * 60
            while (m * 1000 < s.end) {
                val slotStart = maxOf(s.start, m * 1000)
                val slotEnd = minOf(s.end, (m + 60) * 1000)
                val overlap = (slotEnd - slotStart).coerceAtLeast(0)
                byMinute.getOrPut(m) { mutableMapOf() }
                    .merge(s.pkg, overlap) { a, b -> a + b }
                m += 60
            }
        }

        return byMinute.map { (ts, pkgs) ->
            val winner = pkgs.maxByOrNull { it.value }!!.key
            MinuteFrame(
                ts = ts,
                window = "${label(winner)} — $winner",
                apps = pkgs.keys.sorted(),
                // No screen means no way to know how long since a touch; the
                // foreground app changing is the only presence signal there is.
                idleSecs = 0,
                note = null,
            )
        }
    }

    fun query(ctx: Context, from: Long, to: Long): UsageEvents? =
        ctx.getSystemService(UsageStatsManager::class.java)?.queryEvents(from, to)
}
