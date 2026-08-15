package se.frankling.time

import android.content.Context
import android.os.UserManager
import androidx.work.BackoffPolicy
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkerParameters
import androidx.work.WorkManager
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.TimeUnit

object Prefs {
    private const val FILE = "time"
    fun get(ctx: Context) = ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    var Context.server: String
        get() = get(this).getString("server", "https://time.erikfrankling.duckdns.org")!!
        set(v) = get(this).edit().putString("server", v).apply()

    var Context.device: String
        get() = get(this).getString("device", "phone")!!
        set(v) = get(this).edit().putString("device", v).apply()

    /**
     * How far the system event log has been read. Advanced once the minutes
     * are in the spool, not once they are uploaded -- the spool is what an
     * unsent minute now depends on, and it outlives the event log.
     */
    var Context.watermark: Long
        get() = get(this).getLong("watermark", 0)
        set(v) = get(this).edit().putLong("watermark", v).apply()

    var Context.lastSync: Long
        get() = get(this).getLong("lastSync", 0)
        set(v) = get(this).edit().putLong("lastSync", v).apply()

    /** Plain-language cause, the thing the screen leads with. */
    var Context.lastError: String
        get() = get(this).getString("lastError", "")!!
        set(v) = get(this).edit().putString("lastError", v).apply()

    /** The raw code or exception behind it. Kept, but shown small. */
    var Context.lastErrorDetail: String
        get() = get(this).getString("lastErrorDetail", "")!!
        set(v) = get(this).edit().putString("lastErrorDetail", v).apply()

    /** Every run, success or not — so "it is trying" is visible. */
    var Context.lastAttempt: Long
        get() = get(this).getLong("lastAttempt", 0)
        set(v) = get(this).edit().putLong("lastAttempt", v).apply()

    /** Last version the server said it was serving, for the settings screen. */
    var Context.updateVersion: String
        get() = get(this).getString("updateVersion", "")!!
        set(v) = get(this).edit().putString("updateVersion", v).apply()

    var Context.updateCode: Int
        get() = get(this).getInt("updateCode", 0)
        set(v) = get(this).edit().putInt("updateCode", v).apply()

    var Context.updateChecked: Long
        get() = get(this).getLong("updateChecked", 0)
        set(v) = get(this).edit().putLong("updateChecked", v).apply()

    /**
     * The most recent stretch the system event log expired before it could be
     * read — data that is simply gone, recorded so the settings screen can say
     * so instead of the chart being silently blank. Zero when none.
     */
    var Context.lastGapStart: Long
        get() = get(this).getLong("lastGapStart", 0)
        set(v) = get(this).edit().putLong("lastGapStart", v).apply()

    var Context.lastGapEnd: Long
        get() = get(this).getLong("lastGapEnd", 0)
        set(v) = get(this).edit().putLong("lastGapEnd", v).apply()

    /** So the battery-exemption dialog is offered once, not on every open. */
    var Context.askedBatteryExemption: Boolean
        get() = get(this).getBoolean("askedBatteryExemption", false)
        set(v) = get(this).edit().putBoolean("askedBatteryExemption", v).apply()
}

class HttpError(val code: Int) : Exception("HTTP $code")

/**
 * Turn a failure into something the phone's owner can act on.
 *
 * A status code on a settings screen is a dead end: it says a thing went wrong
 * without saying which of the two or three things it actually is. Off the LAN
 * looks like a connect failure from a coffee shop and like a 403 from a network
 * that resolves the name but is not trusted — same cause, same fix, entirely
 * different symptom.
 *
 * Returns (what to tell the user, the raw detail to keep in the small print).
 */
fun describe(e: Exception): Pair<String, String> {
    val detail = e.message ?: e.toString()
    return when {
        e is HttpError && e.code == 403 ->
            "The server refused this device — the ingest route only accepts the LAN " +
                "and the VPN. Connect to either and this clears itself." to detail
        e is HttpError && e.code == 401 ->
            "The server rejected the ingest token." to detail
        e is HttpError && e.code in 500..599 ->
            "The server is reachable but unhealthy. Nothing to do on the phone — " +
                "it keeps retrying." to detail
        e is HttpError && e.code == 404 ->
            "The server URL is wrong, or that server is not running time." to detail
        e is HttpError -> "The server rejected the upload." to detail
        e is java.net.SocketTimeoutException ->
            "The server took too long to answer. It keeps retrying." to detail
        e is java.net.UnknownHostException ->
            "Can't find the server — are you on the LAN or VPN?" to detail
        e is java.net.ConnectException || e is java.net.NoRouteToHostException ->
            "Can't reach the server — are you on the LAN or VPN?" to detail
        e is javax.net.ssl.SSLException ->
            "The server's certificate was rejected." to detail
        e is java.io.IOException ->
            "Network error while uploading. It keeps retrying." to detail
        else -> "Sync failed." to detail
    }
}

/**
 * The sync itself, callable from either scheduler.
 *
 * Two independent things run this: the WorkManager periodic (which survives
 * reboots on its own) and [SyncService]'s loop (which survives the standby
 * bucket throttling WorkManager). Either alone keeps data flowing; the code
 * they share lives here so they cannot drift apart.
 */
object Sync {

    enum class Outcome { SUCCESS, RETRY, FAILURE }

    /** Below the server's per-batch cap, with room to spare. */
    const val CHUNK = 500

    /**
     * The spool is a read-modify-write file and the watermark a read-modify-
     * write pref; two legs running at once would lose whichever write landed
     * second. Both callers are on background threads, so blocking is fine.
     */
    private val lock = Any()

    /**
     * Read the event log, spool the minutes, upload the backlog, then check
     * for updates. May throw — callers own the catch, because what "crashed"
     * means differs between a worker (retry) and a service loop (log, wait).
     */
    fun performSync(ctx: Context): Outcome = synchronized(lock) {
        val outcome = syncUsage(ctx)
        // After the data sync rather than before, so an update-check hiccup —
        // a slow server, a bad JSON body, a notification quirk — can never
        // cost a data sync. Guarded for the same reason: it is decoration on
        // the critical path, not part of it. Still on every run, including
        // failed ones: a phone that cannot sync is exactly the one that most
        // needs the build which might fix it.
        try {
            Update.checkAndNotify(ctx)
        } catch (_: Throwable) {
        }
        outcome
    }

    private fun syncUsage(ctx: Context): Outcome {
        with(Prefs) {
            ctx.lastAttempt = System.currentTimeMillis()

            if (!Usage.hasAccess(ctx)) {
                ctx.lastError = "Usage access is not granted, so there is nothing to send."
                ctx.lastErrorDetail = ""
                return Outcome.FAILURE
            }
            // queryEvents returns null before first unlock on credential-
            // encrypted storage. Retry rather than treat it as no data.
            if (ctx.getSystemService(UserManager::class.java)?.isUserUnlocked == false) {
                return Outcome.RETRY
            }

            val now = System.currentTimeMillis()
            // The system keeps ~7 days; clamp so a long absence doesn't ask
            // for a window that no longer exists. Overlap slightly, because
            // recent events can be truncated, and let the server dedupe on
            // (device, ts).
            // A fresh install has no watermark. Backfilling the full week
            // the system retains means thousands of minutes and a model call
            // for each, to reconstruct days nobody asked about. Start shallow;
            // an existing install keeps its watermark and loses nothing.
            val floor = if (ctx.watermark == 0L) now - 6 * 3600_000L
                        else now - 7 * 24 * 3600_000L
            // A watermark older than the floor means the event log expired
            // before it was read: that stretch is unrecoverable. Say so, on
            // the record, instead of leaving a silent blank on the chart.
            if (ctx.watermark in 1..<floor) {
                ctx.lastGapStart = ctx.watermark
                ctx.lastGapEnd = floor
            }
            val from = maxOf(ctx.watermark - 120_000L, floor)

            val events = Usage.query(ctx, from, now) ?: return Outcome.RETRY
            val fresh = Usage.frames(ctx, Usage.sessions(Usage.drain(events))).map { frame(ctx, it) }

            // Derived minutes go to disk before anything is attempted over the
            // network, and the watermark follows the disk rather than the
            // upload. The system event log is only a write-ahead log for as
            // long as it retains the events; past that it forgets them without
            // saying so, which is a week of silence away from being a week of
            // missing chart.
            val spool = Spool(File(ctx.filesDir, "spool.json"))
            val pending = spool.merge(fresh, now / 1000)
            ctx.watermark = now

            if (pending.isEmpty()) {
                ctx.lastSync = now
                ctx.lastError = ""
                ctx.lastErrorDetail = ""
                return Outcome.SUCCESS
            }

            return try {
                post(ctx, spool, pending)
                ctx.lastSync = now
                ctx.lastError = ""
                ctx.lastErrorDetail = ""
                Outcome.SUCCESS
            } catch (e: Exception) {
                val (why, detail) = describe(e)
                ctx.lastError = why
                ctx.lastErrorDetail = detail

                // Being away from the LAN is the normal state, not a fault, and
                // retrying does not help: the route is unreachable until the
                // phone comes home. Reporting failure would be worse than
                // useless, because WorkManager's exponential backoff replaces
                // the 30-minute period and saturates at five hours after about
                // six consecutive misses -- so a day out of the house would
                // leave the next attempt hours past the point it would have
                // worked. Nothing is lost by waiting: whatever was not accepted
                // is still in the spool.
                if (isUnreachable(e)) Outcome.SUCCESS else Outcome.RETRY
            }
        }
    }

    /**
     * The whole backlog in one request.
     *
     * A phone reports retrospectively, so a normal run has dozens of minutes
     * to send. One request each meant dozens of round trips, each holding a
     * server thread for as long as the model took — which is how the server
     * ran out of threads and started failing its health probes. `/v1/frames`
     * takes the lot as an array. `/v1/frame` stays for the desktop agent,
     * which really does have exactly one minute to send.
     */
    /** One minute in the shape the server's `Frame` expects. */
    private fun frame(ctx: Context, f: MinuteFrame): JSONObject = with(Prefs) {
        JSONObject().apply {
            put("ts", f.ts)
            put("device", ctx.device)
            put("window", f.window)
            put("blocked", false)
            put("keys", 0)
            put("mouse", 0)
            put("workspaces", 0)
            put("apps", JSONArray(f.apps))
            // Omitted rather than zeroed: the field is optional on the server
            // and absence reads as "unknown", which is what a phone knows.
            f.idleSecs?.let { put("idle_secs", it) }
            f.note?.let { put("note", it) }
        }
    }

    private fun post(ctx: Context, spool: Spool, frames: List<JSONObject>) {
        // Chunked because the server caps a batch, and a first sync after a
        // fresh install can hold days of minutes. Sent whole it is rejected,
        // nothing ever drains, and the next run rebuilds the same oversized
        // batch -- a deadlock that never resolves itself.
        //
        // Each chunk leaves the spool as soon as the server has taken it, so an
        // interrupted run resumes rather than replaying the whole backlog.
        for (chunk in frames.chunked(CHUNK)) {
            postChunk(ctx, chunk)
            spool.ack(chunk.map { it.optLong("ts") })
        }
    }

    private fun postChunk(ctx: Context, frames: List<JSONObject>) {
        with(Prefs) {
            val base = ctx.server.trimEnd('/')
            val body = JSONArray(frames).toString()

            val conn = (URL("$base/v1/frames").openConnection() as HttpURLConnection).apply {
                requestMethod = "POST"
                doOutput = true
                connectTimeout = 15_000
                // Ingest stores and queues; it never waits on the model now, so
                // a reply that takes minutes means something is broken rather
                // than merely busy.
                readTimeout = 60_000
                setRequestProperty("Content-Type", "application/json")
            }
            conn.outputStream.use { it.write(body.toByteArray()) }
            val code = conn.responseCode
            conn.disconnect()

            // Any non-2xx leaves the chunk in the spool, to be offered again
            // next run. 403 in particular is not a rejection but a location:
            // the ingest route is lan-only.
            if (code !in 200..299) throw HttpError(code)
        }
    }
}

class SyncWorker(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {

    override suspend fun doWork(): Result {
        val ctx = applicationContext

        // Watchdog leg: the service dies with a force-stop or a rare crash,
        // and nothing else would ever restart it. A background FGS start is
        // allowed here once the battery exemption is granted; without it this
        // throws inside start(), which swallows it — the worker itself is
        // then still the thing keeping data flowing.
        SyncService.start(ctx)

        return try {
            when (Sync.performSync(ctx)) {
                Sync.Outcome.SUCCESS -> Result.success()
                Sync.Outcome.RETRY -> Result.retry()
                Sync.Outcome.FAILURE -> Result.failure()
            }
        } catch (t: Throwable) {
            // Never let a throwable out of doWork. An uncaught one marks the
            // unique periodic spec FAILED, and FAILED is forever: the UPDATE
            // enqueue policy cannot revive a finished spec, so one bad run
            // would silently end all scheduled syncing until the next app
            // update. That is precisely the week-long silence this app is
            // built to make impossible.
            with(Prefs) {
                val (why, detail) =
                    if (t is Exception) describe(t) else "Sync crashed." to t.toString()
                ctx.lastError = why
                ctx.lastErrorDetail = detail
            }
            Result.retry()
        }
    }

    companion object {
        const val PERIODIC = "sync"
        const val ONCE = "sync-now"

        /**
         * A deliberate "Sync now". Unique and REPLACE so repeated taps do not
         * stack up runs, and named so the screen can watch this one request
         * rather than guess from prefs whether anything is happening.
         */
        fun once(ctx: Context) {
            WorkManager.getInstance(ctx).enqueueUniqueWork(
                ONCE,
                ExistingWorkPolicy.REPLACE,
                OneTimeWorkRequestBuilder<SyncWorker>()
                    .setConstraints(
                        Constraints.Builder()
                            .setRequiredNetworkType(NetworkType.CONNECTED)
                            .build()
                    )
                    .build()
            )
        }

        /**
         * 30 minutes with 15 minutes of flex rather than a bare 15, so the
         * system can fold this into a wakeup it was making anyway. Nothing is
         * lost by waiting: queryEvents is retrospective.
         *
         * No network constraint any more. The run's first job is to read the
         * system event log into the spool, and that has to keep happening on a
         * phone with no signal -- the event log is exactly what a week in
         * flight mode would otherwise expire. With nothing reachable the
         * upload half fails on DNS in milliseconds, which `isUnreachable`
         * already treats as "not home" rather than as a fault worth backing
         * off from.
         */
        fun schedule(ctx: Context) {
            val work = PeriodicWorkRequestBuilder<SyncWorker>(
                30, TimeUnit.MINUTES, 15, TimeUnit.MINUTES
            )
                .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 10, TimeUnit.MINUTES)
                .build()

            val wm = WorkManager.getInstance(ctx)
            // A spec that has finished — FAILED from an old uncaught throwable,
            // CANCELLED from anything — is a corpse: UPDATE cannot revive it,
            // and enqueueing against it is a no-op that looks like success.
            // Clear the corpse first, then enqueue. Async (the future's
            // listener) because this runs on the main thread at app open and
            // in the boot receiver; WorkManager serialises its operations, so
            // the cancel lands before the enqueue.
            val infos = wm.getWorkInfosForUniqueWork(PERIODIC)
            infos.addListener({
                val dead = try {
                    infos.get().any { it.state.isFinished }
                } catch (_: Exception) {
                    false
                }
                if (dead) wm.cancelUniqueWork(PERIODIC)
                // UPDATE, not KEEP: KEEP means a build that changes the period or
                // the backoff never reaches a phone that already has the app,
                // which is precisely how a scheduling bug becomes permanent.
                // UPDATE keeps the existing schedule rather than restarting it.
                wm.enqueueUniquePeriodicWork(PERIODIC, ExistingPeriodicWorkPolicy.UPDATE, work)
            }, { it.run() })
        }
    }
}

/**
 * Whether this failure means "not home", as opposed to something worth retrying.
 *
 * A 403 is what the lan-only route returns to the whole internet, so off the LAN
 * it is the expected answer rather than an error. DNS and connect failures mean
 * the same thing when the server only resolves at home.
 */
fun isUnreachable(e: Exception): Boolean =
    (e is HttpError && e.code == 403) ||
        e is java.net.UnknownHostException ||
        e is java.net.ConnectException ||
        e is java.net.NoRouteToHostException
