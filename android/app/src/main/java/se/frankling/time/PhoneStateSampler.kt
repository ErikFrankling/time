package se.frankling.time

import android.app.KeyguardManager
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.net.wifi.WifiInfo
import android.net.wifi.WifiManager
import android.os.BatteryManager
import android.os.PowerManager
import org.json.JSONObject
import java.io.File
import java.net.Inet4Address

/**
 * The phone's state once a minute: charging, battery, locked, screen on,
 * and which network it is on. Sampled by [SyncService]; turned into
 * `time.phone.state` events at the next sync.
 *
 * No GPS and no location data. The network is told apart by its router's
 * address and DNS search domain, which need no permission; the Wi-Fi name is
 * added only if Erik has granted the location permission Android requires
 * for reading it (it never turns location on or reads a position).
 */
object PhoneStateSampler {

    fun sample(ctx: Context, minuteStart: Long): JSONObject {
        val o = JSONObject().put("ts", minuteStart)
        val battery = ctx.registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
        if (battery != null) {
            val status = battery.getIntExtra(BatteryManager.EXTRA_STATUS, -1)
            o.put("charging", status == BatteryManager.BATTERY_STATUS_CHARGING || status == BatteryManager.BATTERY_STATUS_FULL)
            val level = battery.getIntExtra(BatteryManager.EXTRA_LEVEL, -1)
            val scale = battery.getIntExtra(BatteryManager.EXTRA_SCALE, -1)
            if (level >= 0 && scale > 0) o.put("battery", level * 100 / scale)
        }
        o.put("locked", ctx.getSystemService(KeyguardManager::class.java)?.isKeyguardLocked == true)
        o.put("interactive", ctx.getSystemService(PowerManager::class.java)?.isInteractive == true)
        network(ctx, o)
        return o
    }

    private fun network(ctx: Context, o: JSONObject) {
        val cm = ctx.getSystemService(ConnectivityManager::class.java) ?: return
        val net = cm.activeNetwork ?: return
        val caps = cm.getNetworkCapabilities(net) ?: return
        if (!caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)) return
        cm.getLinkProperties(net)?.let { lp ->
            lp.routes.firstOrNull { it.isDefaultRoute && it.gateway is Inet4Address }
                ?.gateway?.hostAddress?.let { o.put("gateway", it) }
            lp.domains?.split(' ', ',')?.firstOrNull { it.isNotBlank() }?.let { o.put("domain", it) }
        }
        ssid(ctx, caps)?.let { o.put("ssid", it) }
    }

    /** Only with the location permission granted; otherwise Android redacts it. */
    private fun ssid(ctx: Context, caps: NetworkCapabilities): String? {
        if (ctx.checkSelfPermission(android.Manifest.permission.ACCESS_FINE_LOCATION) != PackageManager.PERMISSION_GRANTED) return null
        val info = (caps.transportInfo as? WifiInfo)
            ?: @Suppress("DEPRECATION") ctx.getSystemService(WifiManager::class.java)?.connectionInfo
        return info?.ssid?.trim('"')?.takeIf { it.isNotEmpty() && it != WifiManager.UNKNOWN_SSID.trim('<', '>') && it != "<unknown ssid>" }
    }

    fun ssidPermitted(ctx: Context) =
        ctx.checkSelfPermission(android.Manifest.permission.ACCESS_FINE_LOCATION) == PackageManager.PERMISSION_GRANTED
}

/** Samples on disk until the next sync turns them into events. */
class StateSamples(private val file: File) {

    fun add(sample: JSONObject) {
        file.appendText(sample.toString() + "\n")
    }

    fun read(): List<JSONObject> = try {
        if (!file.exists()) emptyList() else file.readLines().mapNotNull { runCatching { JSONObject(it) }.getOrNull() }
    } catch (_: Exception) {
        emptyList()
    }

    fun clear() {
        file.delete()
    }

    companion object {
        fun of(ctx: Context) = StateSamples(File(ctx.filesDir, "state-samples.jsonl"))

        /** One `time.phone.state` event per sampled minute, with a stable id. */
        fun toEvents(device: String, samples: List<JSONObject>): List<Envelope> = samples.map { s ->
            val ts = s.getLong("ts")
            val fields = buildMap<String, Any> {
                put("charging", s.optBoolean("charging"))
                put("locked", s.optBoolean("locked"))
                put("interactive", s.optBoolean("interactive"))
                if (s.has("battery")) put("battery.percent", s.getInt("battery").toLong())
                if (s.has("ssid")) put("wifi.ssid", s.getString("ssid"))
                if (s.has("gateway")) put("network.gateway", s.getString("gateway"))
                if (s.has("domain")) put("network.domain", s.getString("domain"))
            }
            Envelope(Envelope.stableId(device, "time.phone.state", ts, ts + 60_000), "time.phone.state", ts, ts + 60_000, fields)
        }
    }
}
