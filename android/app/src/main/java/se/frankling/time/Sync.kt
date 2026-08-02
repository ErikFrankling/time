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

class SyncWorker(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {

    override suspend fun doWork(): Result {
        val ctx = applicationContext

        // Before the usage-access gate on purpose: a phone that cannot sync is
        // exactly the one that most needs the build which might fix it.
        Update.checkAndNotify(ctx)

        with(Prefs) {
            ctx.lastAttempt = System.currentTimeMillis()

            if (!Usage.hasAccess(ctx)) {
                ctx.lastError = "Usage access is not granted, so there is nothing to send."
                ctx.lastErrorDetail = ""
                return Result.failure()
            }
            // queryEvents returns null before first unlock on credential-
            // encrypted storage. Retry rather than treat it as no data.
            if (ctx.getSystemService(UserManager::class.java)?.isUserUnlocked == false) {
                return Result.retry()
            }

            val now = System.currentTimeMillis()
            // The system keeps ~7 days; clamp so a long absence doesn't ask
            // for a window that no longer exists. Overlap slightly, because
            // recent events can be truncated, and let the server dedupe on
            // (device, ts).
            // A fresh install has no watermark. Backfilling the full six days
            // the system retains means thousands of minutes and a model call
            // for each, to reconstruct days nobody asked about. Start shallow;
            // an existing install keeps its watermark and loses nothing.
            val floor = if (ctx.watermark == 0L) now - 6 * 3600_000L
                        else now - 6 * 24 * 3600_000L
            val from = maxOf(ctx.watermark - 120_000L, floor)

            val events = Usage.query(ctx, from, now) ?: return Result.retry()
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
                return Result.success()
            }

            return try {
                post(ctx, spool, pending)
                ctx.lastSync = now
                ctx.lastError = ""
                ctx.lastErrorDetail = ""
                Result.success()
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
                if (isUnreachable(e)) Result.success() else Result.retry()
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

    companion object {
        /** Below the server's per-batch cap, with room to spare. */
        const val CHUNK = 500

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

            WorkManager.getInstance(ctx)
                // UPDATE, not KEEP: KEEP means a build that changes the period or
                // the backoff never reaches a phone that already has the app,
                // which is precisely how a scheduling bug becomes permanent.
                // UPDATE keeps the existing schedule rather than restarting it.
                .enqueueUniquePeriodicWork(PERIODIC, ExistingPeriodicWorkPolicy.UPDATE, work)
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
