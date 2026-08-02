package se.frankling.time

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.net.Uri
import android.os.Build
import android.provider.Settings
import androidx.core.content.FileProvider
import org.json.JSONObject
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest

/**
 * Self-update over the same server the frames go to.
 *
 * There is no store behind this app, so nothing else will ever tell it a new
 * build exists. What it cannot do is install silently: that needs a system or
 * privileged signature, so every update ends at a confirmation dialog the user
 * taps. The goal is that the user never has to *think* about it, not that they
 * never touch it.
 */
object Update {

    private const val CHANNEL = "updates"
    private const val NOTIFICATION = 1

    /** What the server says is available, as returned by `GET /app/version`. */
    data class Available(
        val version: String,
        val versionCode: Int,
        val sha256: String,
        val url: String,
        val published: String,
        val signing: String,
    ) {
        val newer: Boolean get() = versionCode > BuildConfig.VERSION_CODE
    }

    fun parse(json: String): Available {
        val o = JSONObject(json)
        return Available(
            version = o.optString("version", "?"),
            versionCode = o.optInt("versionCode", 0),
            sha256 = o.optString("sha256", ""),
            // Relative by design: the server hands out its own path, because the
            // phone may be on a LAN with no route to GitHub.
            url = o.optString("url", "/app"),
            published = o.optString("published", ""),
            signing = o.optString("signing", "unknown"),
        )
    }

    /** Ask the server what it is serving. Returns null on any failure. */
    fun check(ctx: Context): Available? = with(Prefs) {
        return try {
            val base = ctx.server.trimEnd('/')
            val conn = (URL("$base/app/version").openConnection() as HttpURLConnection).apply {
                connectTimeout = 10_000
                readTimeout = 15_000
            }
            val body = try {
                if (conn.responseCode !in 200..299) return null
                conn.inputStream.bufferedReader().readText()
            } finally {
                conn.disconnect()
            }
            parse(body).also {
                ctx.updateVersion = it.version
                ctx.updateCode = it.versionCode
                ctx.updateChecked = System.currentTimeMillis()
            }
        } catch (e: Exception) {
            null
        }
    }

    /**
     * Download to the app cache and verify. The cache is the right home: if the
     * install never happens the OS reclaims it, and there is nothing here worth
     * keeping once the new version is running.
     */
    fun download(ctx: Context, available: Available): File? {
        val dir = File(ctx.cacheDir, "updates").apply { mkdirs() }
        val target = File(dir, "time-${available.versionCode}.apk")
        if (target.exists() && sha256(target) == available.sha256) return target

        // Anything from an older check is dead weight the moment a new build
        // lands, and these are 20+ MB each.
        dir.listFiles()?.forEach { if (it != target) it.delete() }

        val base = with(Prefs) { ctx.server.trimEnd('/') }
        val url = if (available.url.startsWith("http")) available.url else base + available.url
        val part = File(dir, target.name + ".part")
        return try {
            val conn = (URL(url).openConnection() as HttpURLConnection).apply {
                connectTimeout = 15_000
                readTimeout = 120_000
            }
            try {
                if (conn.responseCode !in 200..299) return null
                conn.inputStream.use { input -> part.outputStream().use { input.copyTo(it) } }
            } finally {
                conn.disconnect()
            }
            // A truncated APK fails at the installer with a useless error. Catch
            // it here, where the message can say what actually happened.
            if (available.sha256.isNotEmpty() && sha256(part) != available.sha256) {
                part.delete()
                return null
            }
            if (part.renameTo(target)) target else null
        } catch (e: Exception) {
            part.delete()
            null
        }
    }

    /** True once the user has allowed this app to install packages. */
    fun canInstall(ctx: Context): Boolean = ctx.packageManager.canRequestPackageInstalls()

    /** Settings screen for granting the above. */
    fun installPermissionIntent(ctx: Context): Intent =
        Intent(
            Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
            // Without the package: URI this lands on the full list of apps and
            // the user has to find this one.
            Uri.parse("package:${ctx.packageName}"),
        )

    /**
     * Hand the APK to the system package installer.
     *
     * ACTION_VIEW rather than PackageInstaller: both arrive at exactly the same
     * confirmation dialog, and the session API costs a broadcast receiver, a
     * status callback and a chunk of stream plumbing to get there.
     */
    fun installIntent(ctx: Context, apk: File): Intent {
        val uri = FileProvider.getUriForFile(ctx, "${ctx.packageName}.updates", apk)
        return Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(uri, "application/vnd.android.package-archive")
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        }
    }

    /**
     * The whole background path: check, download, notify. Called from the sync
     * worker, and deliberately silent on failure — an unreachable server should
     * not produce a notification, it should produce nothing.
     */
    fun checkAndNotify(ctx: Context, allowMetered: Boolean = false) {
        val available = check(ctx) ?: return
        if (!available.newer) return
        // ~25 MB unannounced on someone's cellular data is not a thing to do on
        // a schedule. The version is already recorded, so the settings screen
        // offers it and a deliberate tap downloads it anywhere.
        if (!allowMetered && !unmetered(ctx)) return
        val apk = download(ctx, available) ?: return
        notify(ctx, available, apk)
    }

    private fun unmetered(ctx: Context): Boolean {
        val cm = ctx.getSystemService(ConnectivityManager::class.java) ?: return false
        val caps = cm.getNetworkCapabilities(cm.activeNetwork) ?: return false
        return caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)
    }

    private fun notify(ctx: Context, available: Available, apk: File) {
        if (Build.VERSION.SDK_INT >= 33 &&
            ctx.checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            // The settings screen still shows the update, so nothing is lost.
            return
        }

        val nm = ctx.getSystemService(NotificationManager::class.java) ?: return
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL, "Updates", NotificationManager.IMPORTANCE_DEFAULT)
        )

        // Tapping has to lead somewhere useful either way: straight to the
        // installer if that is possible, otherwise to the one settings screen
        // that makes it possible.
        val ready = canInstall(ctx)
        val intent = if (ready) installIntent(ctx, apk) else installPermissionIntent(ctx)
        val pending = PendingIntent.getActivity(
            ctx,
            available.versionCode,
            intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )

        nm.notify(
            NOTIFICATION,
            android.app.Notification.Builder(ctx, CHANNEL)
                .setSmallIcon(android.R.drawable.stat_sys_download_done)
                .setContentTitle("time v${available.version} available")
                .setContentText(
                    if (ready) "Tap to install" else "Tap to allow installing updates"
                )
                .setAutoCancel(true)
                .setContentIntent(pending)
                .build()
        )
    }

    fun sha256(f: File): String {
        val md = MessageDigest.getInstance("SHA-256")
        f.inputStream().use { input ->
            val buf = ByteArray(64 * 1024)
            while (true) {
                val n = input.read(buf)
                if (n <= 0) break
                md.update(buf, 0, n)
            }
        }
        return hex(md.digest())
    }

    fun hex(bytes: ByteArray): String {
        val out = StringBuilder(bytes.size * 2)
        for (b in bytes) out.append("%02x".format(b))
        return out.toString()
    }
}
