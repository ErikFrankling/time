package se.frankling.time

import android.app.AppOpsManager
import android.app.usage.UsageEvents
import android.app.usage.UsageStatsManager
import android.content.Context
import android.content.pm.PackageManager
import android.os.Process

/** A contiguous stretch with one app in the foreground. */
data class Session(val pkg: String, val start: Long, val end: Long)

/**
 * One system event, flattened.
 *
 * UsageEvents cannot be constructed outside the framework, so the state machine
 * below reads this instead and stays testable on a plain JVM. Getting that logic
 * wrong is the difference between a real day and a fabricated one, so it has to
 * be checkable without an emulator.
 */
data class Ev(val type: Int, val pkg: String, val ts: Long)


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
    fun sessions(events: List<Ev>): List<Session> {
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

        for (e in events) {
            when (e.type) {
                UsageEvents.Event.ACTIVITY_RESUMED -> {
                    // Re-resume of the same app keeps the earlier start.
                    if (openPkg == e.pkg) continue
                    close(e.ts)
                    openPkg = e.pkg
                    openStart = e.ts
                }
                UsageEvents.Event.ACTIVITY_PAUSED ->
                    if (openPkg == e.pkg) close(e.ts)
                UsageEvents.Event.SCREEN_NON_INTERACTIVE,
                UsageEvents.Event.KEYGUARD_SHOWN,
                UsageEvents.Event.DEVICE_SHUTDOWN -> close(e.ts)
            }
        }
        return out
    }

    /** Drain the framework's cursor into something the state machine can read. */
    fun drain(events: UsageEvents): List<Ev> {
        val out = mutableListOf<Ev>()
        val e = UsageEvents.Event()
        while (events.hasNextEvent()) {
            events.getNextEvent(e)
            out.add(Ev(e.eventType, e.packageName, e.timeStamp))
        }
        return out
    }

    /**
     * Sessions as events. The server turns spans into minutes; doing it here
     * (as v1 did) threw away the exact boundaries and multiplied the data.
     */
    fun spans(ctx: Context, device: String, sessions: List<Session>, media: Set<Long>): List<Envelope> {
        val pm = ctx.packageManager
        val labels = mutableMapOf<String, String>()
        fun label(pkg: String) = labels.getOrPut(pkg) {
            try {
                pm.getApplicationLabel(pm.getApplicationInfo(pkg, 0)).toString()
            } catch (_: Exception) {
                pkg
            }
        }
        return sessions.map { s ->
            Envelope.appSpan(device, s.pkg, label(s.pkg), s.start, s.end, MediaSamples.mostlyPlaying(media, s.start, s.end))
        }
    }

    fun query(ctx: Context, from: Long, to: Long): UsageEvents? =
        ctx.getSystemService(UsageStatsManager::class.java)?.queryEvents(from, to)
}
