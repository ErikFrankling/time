package se.frankling.time

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.ServiceCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

/**
 * The always-on leg of the sync, in the shape Syncthing-Fork proved out:
 * a specialUse foreground service, START_STICKY, an ongoing minimum-importance
 * notification, started again by boot, app update, app open and the periodic
 * worker.
 *
 * This exists because WorkManager alone was not enough. A periodic job lives
 * inside the app-standby regime: an app that is not opened decays to the RARE
 * bucket where the quota is three job sessions a day, and a force-stop or a
 * FAILED spec silences it entirely with nothing left to notice. A foreground
 * service is outside that regime — Android will not bucket-throttle it, and
 * START_STICKY has the system itself restart it after a process death. The
 * price is one silent notification.
 *
 * specialUse and not dataSync on purpose: dataSync gets a hard 6-hour runtime
 * cap and is banned from BOOT_COMPLETED starts on Android 15; specialUse has
 * neither restriction.
 */
class SyncService : Service() {

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var loop: Job? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Every startForegroundService() call must be answered with a
        // startForeground(), even when the service was already running.
        // Doubles as the notification refresh after a worker-leg sync, since
        // the worker pokes start() on every run.
        foreground()

        if (loop?.isActive != true) {
            loop = scope.launch {
                while (isActive) {
                    try {
                        Sync.performSync(this@SyncService)
                    } catch (t: Throwable) {
                        // Same contract as the worker: nothing escapes. A
                        // throwable here would kill the loop while the service
                        // kept looking alive — worse than the crash.
                        with(Prefs) {
                            val (why, detail) = if (t is Exception) describe(t)
                                                else "Sync crashed." to t.toString()
                            lastError = why
                            lastErrorDetail = detail
                        }
                    }
                    notifyStatus()
                    delay(PERIOD_MS)
                }
            }
        }
        // START_STICKY: if the process is killed the system recreates the
        // service with a null intent, which lands right back here and
        // restarts the loop.
        return START_STICKY
    }

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }

    private fun foreground() {
        val n = build()
        if (Build.VERSION.SDK_INT >= 34) {
            // From API 34 the type must be passed at startForeground time and
            // is checked against the manifest declaration.
            ServiceCompat.startForeground(
                this, ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE
            )
        } else {
            startForeground(ID, n)
        }
    }

    private fun notifyStatus() {
        getSystemService(NotificationManager::class.java)?.notify(ID, build())
    }

    /**
     * IMPORTANCE_MIN, no badge: the quietest thing Android allows an FGS to
     * post — collapsed at the bottom of the shade, no sound, no status-bar
     * icon on most builds. It has to exist; it does not have to nag.
     */
    private fun build(): Notification {
        getSystemService(NotificationManager::class.java)?.createNotificationChannel(
            NotificationChannel(
                CHANNEL, "Tracking", NotificationManager.IMPORTANCE_MIN
            ).apply { setShowBadge(false) }
        )

        val last = with(Prefs) { lastSync }
        val text = if (last == 0L) "tracking · no sync yet" else {
            val min = (System.currentTimeMillis() - last) / 60_000
            when {
                min < 1 -> "tracking · synced just now"
                min < 60 -> "tracking · last sync $min min ago"
                min < 48 * 60 -> "tracking · last sync ${min / 60} h ago"
                else -> "tracking · last sync ${min / (24 * 60)} days ago"
            }
        }

        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
        return Notification.Builder(this, CHANNEL)
            .setSmallIcon(android.R.drawable.stat_notify_sync_noanim)
            .setContentTitle("time")
            .setContentText(text)
            .setOngoing(true)
            .setContentIntent(open)
            .build()
    }

    companion object {
        private const val CHANNEL = "tracking"

        /** Update.kt owns notification id 1. */
        private const val ID = 2

        private const val PERIOD_MS = 30 * 60_000L

        /**
         * Start the service, or poke a running one into refreshing its
         * notification. Callable from anywhere: the contexts this app starts
         * it from are either exempt (BOOT_COMPLETED, MY_PACKAGE_REPLACED, a
         * foreground activity) or allowed by the battery exemption (the
         * periodic worker). Where none of that holds Android throws, and the
         * right answer is to shrug — the worker leg still carries the data,
         * and the next open or boot starts the service properly.
         */
        fun start(ctx: Context) {
            try {
                ctx.startForegroundService(Intent(ctx, SyncService::class.java))
            } catch (_: Exception) {
            }
        }
    }
}
