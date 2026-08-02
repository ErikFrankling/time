package se.frankling.time

import android.content.Context
import android.os.UserManager
import androidx.work.BackoffPolicy
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.NetworkType
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkerParameters
import androidx.work.WorkManager
import org.json.JSONArray
import org.json.JSONObject
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

    /** Only advanced on a 2xx, so a failed upload replays next run. */
    var Context.watermark: Long
        get() = get(this).getLong("watermark", 0)
        set(v) = get(this).edit().putLong("watermark", v).apply()

    var Context.lastSync: Long
        get() = get(this).getLong("lastSync", 0)
        set(v) = get(this).edit().putLong("lastSync", v).apply()

    var Context.lastError: String
        get() = get(this).getString("lastError", "")!!
        set(v) = get(this).edit().putString("lastError", v).apply()
}

class SyncWorker(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {

    override suspend fun doWork(): Result {
        val ctx = applicationContext
        with(Prefs) {
            if (!Usage.hasAccess(ctx)) {
                ctx.lastError = "usage access not granted"
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
            val floor = now - 6 * 24 * 3600_000L
            val from = maxOf(ctx.watermark - 120_000L, floor)

            val events = Usage.query(ctx, from, now) ?: return Result.retry()
            val frames = Usage.frames(ctx, Usage.sessions(events))
            if (frames.isEmpty()) {
                ctx.lastSync = now
                return Result.success()
            }

            return try {
                post(ctx, frames)
                // Advance only on success. The system event log is the
                // write-ahead log, so there is no local buffer to lose.
                ctx.watermark = now
                ctx.lastSync = now
                ctx.lastError = ""
                Result.success()
            } catch (e: Exception) {
                ctx.lastError = e.message ?: e.toString()
                Result.retry()
            }
        }
    }

    private fun post(ctx: Context, frames: List<MinuteFrame>) {
        with(Prefs) {
            val base = ctx.server.trimEnd('/')
            for (f in frames) {
                val body = JSONObject().apply {
                    put("ts", f.ts)
                    put("device", ctx.device)
                    put("window", f.window)
                    put("blocked", false)
                    put("keys", 0)
                    put("mouse", 0)
                    put("idle_secs", f.idleSecs)
                    put("workspaces", 0)
                    put("apps", JSONArray(f.apps))
                    f.note?.let { put("note", it) }
                }.toString()

                val conn = (URL("$base/v1/frame").openConnection() as HttpURLConnection).apply {
                    requestMethod = "POST"
                    doOutput = true
                    connectTimeout = 15_000
                    // The server may take a while when it calls the model.
                    readTimeout = 180_000
                    setRequestProperty("Content-Type", "application/json")
                }
                conn.outputStream.use { it.write(body.toByteArray()) }
                val code = conn.responseCode
                conn.disconnect()

                // 403 means off-LAN, not rejected: the route is lan-only.
                // Treat it like any other failure so the watermark holds and
                // the same minutes replay once we're home.
                if (code !in 200..299) error("HTTP $code")
            }
        }
    }

    companion object {
        /**
         * 30 minutes with 15 minutes of flex rather than a bare 15, so the
         * system can fold this into a wakeup it was making anyway. Nothing is
         * lost by waiting: queryEvents is retrospective.
         */
        fun schedule(ctx: Context) {
            val work = PeriodicWorkRequestBuilder<SyncWorker>(
                30, TimeUnit.MINUTES, 15, TimeUnit.MINUTES
            )
                .setConstraints(
                    Constraints.Builder()
                        .setRequiredNetworkType(NetworkType.CONNECTED)
                        .build()
                )
                .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 10, TimeUnit.MINUTES)
                .build()

            WorkManager.getInstance(ctx)
                .enqueueUniquePeriodicWork("sync", ExistingPeriodicWorkPolicy.KEEP, work)
        }
    }
}
