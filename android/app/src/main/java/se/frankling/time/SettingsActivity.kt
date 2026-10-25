package se.frankling.time

import android.app.usage.UsageStatsManager
import android.net.Uri
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.PowerManager
import android.os.Bundle
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.core.content.IntentCompat
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.work.WorkInfo
import androidx.work.WorkManager
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import se.frankling.time.Prefs.askedBatteryExemption
import se.frankling.time.Prefs.device
import se.frankling.time.Prefs.lastAttempt
import se.frankling.time.Prefs.lastError
import se.frankling.time.Prefs.lastErrorDetail
import se.frankling.time.Prefs.lastGapEnd
import se.frankling.time.Prefs.lastGapStart
import se.frankling.time.Prefs.lastSync
import se.frankling.time.Prefs.server
import se.frankling.time.Prefs.otlpEndpoint
import se.frankling.time.Prefs.token
import androidx.health.connect.client.PermissionController

class SettingsActivity : ComponentActivity() {

    private val askNotifications =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { }

    // Optional: Wi-Fi names (Android only reveals them with this grant).
    private val askLocation =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { }

    // Health Connect has its own grant flow; the result is re-read on resume.
    private val askHealth =
        registerForActivityResult(PermissionController.createRequestPermissionResultContract()) { }

    // The hibernation screen is documented as needing startActivityForResult;
    // the result itself is meaningless, the screen state is re-read on resume.
    private val hibernationSettings =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { }

    override fun onCreate(savedInstanceState: Bundle?) {
        // Edge to edge like the main screen, content kept clear of the bars.
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
        )
        super.onCreate(savedInstanceState)
        // Both legs, every open: the worker in case its spec died, the service
        // in case a force-stop or crash took it down. A foreground activity is
        // always an allowed start context.
        SyncWorker.schedule(this)
        SyncService.start(this)

        // The update prompt is the only notification this app posts, and the
        // grant is cheap to ask for once at first launch.
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            askNotifications.launch(android.Manifest.permission.POST_NOTIFICATIONS)
        }

        // First run: offer the battery exemption straight away rather than as
        // a button someone has to notice. It is what lets the worker restart
        // the service from the background and what stops standby-bucket decay.
        // Once — a declined dialog re-shown on every open is nagware.
        val pm = getSystemService(PowerManager::class.java)
        if (pm?.isIgnoringBatteryOptimizations(packageName) == false && !askedBatteryExemption) {
            askedBatteryExemption = true
            try {
                startActivity(
                    Intent(
                        Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                        Uri.parse("package:$packageName")
                    )
                )
            } catch (_: Exception) {
            }
        }

        // Any stale state — first run, a dead schedule, a week in a drawer —
        // triggers an immediate catch-up rather than waiting for the periodic.
        // 35 minutes: one period plus slack, so a healthy schedule never trips
        // it and anything unhealthy heals the moment the app is opened.
        if (lastSync < System.currentTimeMillis() - 35 * 60_000L && Usage.hasAccess(this)) {
            SyncWorker.once(this)
        }

        // Dark, always: Erik never wants a white screen.
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                Surface(color = MaterialTheme.colorScheme.background) { Screen() }
            }
        }
    }

    @Composable
    private fun Screen() {
        var srv by remember { mutableStateOf(server) }
        var dev by remember { mutableStateOf(device) }
        var otlp by remember { mutableStateOf(otlpEndpoint) }
        var tok by remember { mutableStateOf(token) }

        val wm = remember { WorkManager.getInstance(this) }
        val once by wm.getWorkInfosForUniqueWorkFlow(SyncWorker.ONCE)
            .collectAsState(initial = emptyList())
        val periodic by wm.getWorkInfosForUniqueWorkFlow(SyncWorker.PERIODIC)
            .collectAsState(initial = emptyList())

        // Prefs have no change stream, so they are re-read whenever the work
        // state moves or the screen comes back to the foreground. That covers
        // every moment they can actually have changed, and removes the
        // "Refresh status" button that used to stand in for it.
        var resumed by remember { mutableIntStateOf(0) }
        OnResume { resumed++ }
        val key = Triple(once.firstOrNull()?.state, periodic.firstOrNull()?.state, resumed)

        val granted = remember(key) { Usage.hasAccess(this) }
        val batteryExempt = remember(key) {
            getSystemService(PowerManager::class.java)
                ?.isIgnoringBatteryOptimizations(packageName) ?: true
        }
        val synced = remember(key) { lastSync }
        val attempted = remember(key) { lastAttempt }
        val err = remember(key) { lastError }
        val detail = remember(key) { lastErrorDetail }
        val gapStart = remember(key) { lastGapStart }
        val gapEnd = remember(key) { lastGapEnd }
        val bucket = remember(key) {
            try {
                getSystemService(UsageStatsManager::class.java)?.appStandbyBucket
            } catch (_: Exception) {
                null
            }
        }

        val running = once.any { it.state == WorkInfo.State.RUNNING } ||
            periodic.any { it.state == WorkInfo.State.RUNNING }
        val nextRun = periodic.firstOrNull()?.nextScheduleTimeMillis
            ?.takeIf { it in 1..<Long.MAX_VALUE }

        Column(
            Modifier.fillMaxSize()
                .windowInsetsPadding(WindowInsets.safeDrawing)
                .verticalScroll(rememberScrollState())
                .padding(24.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp)
        ) {
            Text("Settings", style = MaterialTheme.typography.headlineMedium)
            Text(
                "Sends which app is in the foreground, and steps from Health Connect, " +
                    "every 15 minutes. No screenshots, no location.",
                style = MaterialTheme.typography.bodyMedium
            )

            OutlinedTextField(
                value = srv, onValueChange = { srv = it; server = it },
                label = { Text("Server") }, singleLine = true,
                modifier = Modifier.fillMaxWidth()
            )
            OutlinedTextField(
                value = dev, onValueChange = { dev = it; device = it },
                label = { Text("Device name") }, singleLine = true,
                modifier = Modifier.fillMaxWidth()
            )
            OutlinedTextField(
                value = otlp, onValueChange = { otlp = it; otlpEndpoint = it },
                label = { Text("Event gateway (OTLP)") }, singleLine = true,
                modifier = Modifier.fillMaxWidth()
            )
            OutlinedTextField(
                value = tok, onValueChange = { tok = it; token = it },
                label = { Text("Gateway token") }, singleLine = true,
                visualTransformation = androidx.compose.ui.text.input.PasswordVisualTransformation(),
                modifier = Modifier.fillMaxWidth()
            )

            HorizontalDivider()

            // Usage access is the one thing that silently breaks everything,
            // so its state is the most prominent thing on the screen.
            Text(
                if (granted) "Usage access: granted" else "Usage access: NOT granted",
                style = MaterialTheme.typography.titleMedium
            )
            if (!granted) {
                Button(
                    onClick = { startActivity(Intent(Settings.ACTION_USAGE_ACCESS_SETTINGS)) },
                    modifier = Modifier.fillMaxWidth()
                ) { Text("Grant usage access") }
                // Android 13+ marks apps installed by a plain browser download
                // as untrusted and greys out restricted permissions, of which
                // PACKAGE_USAGE_STATS is one. The toggle then does nothing and
                // gives no reason, so the reason has to live here. There is no
                // API to detect the state, so it is always shown.
                Text(
                    "If the toggle refuses to turn on: Settings → Apps → time → ⋮ → " +
                        "Allow restricted settings, then come back. Android hides that " +
                        "menu item until it has blocked the permission at least once. " +
                        "Installing through Obtainium avoids this entirely.",
                    style = MaterialTheme.typography.bodySmall
                )
            }

            HorizontalDivider()

            // Places: the router address works without any grant; the Wi-Fi
            // name needs Android's location permission (no GPS is used).
            val ssidOk = remember(key) { PhoneStateSampler.ssidPermitted(this@SettingsActivity) }
            Text(
                if (ssidOk) "Wi-Fi names: shown" else "Wi-Fi names: hidden (places still work by router)",
                style = MaterialTheme.typography.titleMedium
            )
            if (!ssidOk) {
                OutlinedButton(
                    onClick = { askLocation.launch(android.Manifest.permission.ACCESS_FINE_LOCATION) },
                    modifier = Modifier.fillMaxWidth()
                ) { Text("Show Wi-Fi names (eduroam, home)") }
                Text(
                    "Android only reveals the network name to apps with the location permission. " +
                        "This app never turns location on or reads a position.",
                    style = MaterialTheme.typography.bodySmall
                )
            }

            HorizontalDivider()

            // Track names for what plays on the phone: media sessions are only
            // handed to an app with notification access.
            val sessionsOk = remember(key) { PhoneAudio.hasSessionAccess(this@SettingsActivity) }
            Text(
                if (sessionsOk) "What is playing: named" else "What is playing: app on screen only",
                style = MaterialTheme.typography.titleMedium
            )
            if (!sessionsOk) {
                OutlinedButton(
                    onClick = {
                        startActivity(Intent(android.provider.Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS))
                    },
                    modifier = Modifier.fillMaxWidth()
                ) { Text("Name what is playing (Spotify, YouTube)") }
                Text(
                    "Turn on \"Time: what is playing\" under notification access. It lets Time see which app " +
                        "and track are playing while the sound is on. Notifications themselves are never read.",
                    style = MaterialTheme.typography.bodySmall
                )
            }

            HorizontalDivider()

            // Steps and distance. Health Connect is filled by what the phone
            // already runs; this app never uses GPS or asks for location.
            val healthMissing = remember(key) { Health.missing(this@SettingsActivity) }
            Text(
                if (healthMissing.isEmpty()) "Steps and distance: granted" else "Steps and distance: NOT fully granted",
                style = MaterialTheme.typography.titleMedium
            )
            for (m in healthMissing) Text(m, style = MaterialTheme.typography.bodySmall)
            if (healthMissing.isNotEmpty() && Health.available(this@SettingsActivity)) {
                Button(
                    onClick = { askHealth.launch(Health.PERMISSIONS + Health.BACKGROUND) },
                    modifier = Modifier.fillMaxWidth()
                ) { Text("Grant Health Connect access") }
            }

            HorizontalDivider()

            SyncStatus(running, granted, synced, attempted, err, detail, nextRun, gapStart, gapEnd)

            HorizontalDivider()

            // The states that decide whether background sync actually happens,
            // all in one place — this is the page that gets read when the
            // chart has a hole in it.
            Text("Background health", style = MaterialTheme.typography.titleMedium)
            Text(
                "Standby bucket: ${bucketName(bucket)}",
                style = MaterialTheme.typography.bodyMedium
            )
            Text(
                if (batteryExempt) "Battery exemption: granted"
                else "Battery exemption: NOT granted",
                style = MaterialTheme.typography.bodyMedium
            )

            // The single biggest thing available for keeping the schedule
            // honest. An app opened once a week decays to the RARE standby
            // bucket, where the whole quota is three job sessions a day -- so
            // a thirty-minute period quietly becomes eight-hourly. The
            // exemption pins the app to EXEMPTED, which stops the decay, lets
            // jobs run through Doze, and allows the worker to restart the
            // sync service from the background.
            if (!batteryExempt) {
                OutlinedButton(
                    onClick = {
                        startActivity(
                            Intent(
                                Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                                Uri.parse("package:$packageName")
                            )
                        )
                    },
                    modifier = Modifier.fillMaxWidth()
                ) { Text("Sync more often (exempt from battery limits)") }
                Text(
                    "Without it Android throttles the sync to roughly three " +
                        "times a day once the app has gone a couple of days unopened, " +
                        "and the sync service cannot be restarted from the background.",
                    style = MaterialTheme.typography.bodySmall
                )
            }

            // Hibernation is the slowest of the killers: months unopened and
            // Android revokes every permission — usage access included — and
            // force-stops the app. There is no API to read or clear the state
            // directly, only this settings screen.
            OutlinedButton(
                onClick = {
                    try {
                        hibernationSettings.launch(
                            IntentCompat.createManageUnusedAppRestrictionsIntent(
                                this@SettingsActivity, packageName
                            )
                        )
                    } catch (_: Exception) {
                    }
                },
                modifier = Modifier.fillMaxWidth()
            ) { Text("Exempt from app hibernation") }
            Text(
                "Turn off “Pause app activity if unused”, or months of " +
                    "not opening this screen ends with Android revoking usage access.",
                style = MaterialTheme.typography.bodySmall
            )

            Button(
                onClick = { SyncWorker.once(this@SettingsActivity) },
                enabled = !running,
                modifier = Modifier.fillMaxWidth()
            ) {
                if (running) {
                    Row(
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                        verticalAlignment = androidx.compose.ui.Alignment.CenterVertically
                    ) {
                        CircularProgressIndicator(
                            Modifier.size(16.dp),
                            strokeWidth = 2.dp,
                            color = MaterialTheme.colorScheme.onPrimary
                        )
                        Text("Syncing…")
                    }
                } else {
                    Text("Sync now")
                }
            }

            HorizontalDivider()

            Updates()
        }
    }

    /** "2 min ago (Aug 15, 14:02)" — relative first, absolute for the record. */
    private fun ago(t: Long): String {
        val full = SimpleDateFormat("MMM d, HH:mm", Locale.getDefault()).format(Date(t))
        val d = System.currentTimeMillis() - t
        val rel = when {
            d < 60_000L -> "just now"
            d < 3600_000L -> "${d / 60_000} min ago"
            d < 24 * 3600_000L -> "${d / 3600_000} h ago"
            else -> "${d / (24 * 3600_000L)} days ago"
        }
        return "$rel ($full)"
    }

    private fun bucketName(bucket: Int?): String = when (bucket) {
        null -> "unknown"
        // 5 is STANDBY_BUCKET_EXEMPTED, @SystemApi so no public constant, but
        // it is what the battery exemption pins the app to and worth naming.
        5 -> "exempted — never throttled"
        UsageStatsManager.STANDBY_BUCKET_ACTIVE -> "active"
        UsageStatsManager.STANDBY_BUCKET_WORKING_SET -> "working set"
        UsageStatsManager.STANDBY_BUCKET_FREQUENT -> "frequent"
        UsageStatsManager.STANDBY_BUCKET_RARE -> "rare — jobs throttled to ~3/day"
        UsageStatsManager.STANDBY_BUCKET_RESTRICTED -> "restricted — jobs ~1/day"
        else -> "$bucket"
    }

    @Composable
    private fun SyncStatus(
        running: Boolean,
        granted: Boolean,
        synced: Long,
        attempted: Long,
        err: String,
        detail: String,
        nextRun: Long?,
        gapStart: Long,
        gapEnd: Long,
    ) {
        val stamp = { t: Long ->
            SimpleDateFormat("HH:mm", Locale.getDefault()).format(Date(t))
        }
        val day = { t: Long ->
            SimpleDateFormat("MMM d HH:mm", Locale.getDefault()).format(Date(t))
        }

        // Two hours is four missed periods across both legs — no honest way
        // to call that anything but broken, and pretending otherwise is how
        // the last silent week happened.
        val stale = !running && granted && synced > 0 &&
            System.currentTimeMillis() - synced > 2 * 3600_000L

        val headline = when {
            running -> "Syncing…"
            !granted -> "Paused — usage access is off"
            err.isNotEmpty() -> err
            synced == 0L && attempted == 0L -> "Waiting for the first sync"
            synced == 0L -> "No data reported yet"
            stale -> "STALLED — last sync ${ago(synced)}"
            else -> "Working — last sync ${ago(synced)}"
        }

        Text("Status", style = MaterialTheme.typography.titleMedium)
        Text(
            headline,
            style = MaterialTheme.typography.bodyLarge,
            color = if (stale) MaterialTheme.colorScheme.error else Color.Unspecified
        )

        // "Last error" with nothing after it reads like a dead end. It is not:
        // WorkManager backs off and tries again on its own, and saying when
        // turns a fault into a wait.
        if (err.isNotEmpty() && !running) {
            Text(
                "Retrying by itself" + (nextRun?.let { ", next attempt around ${stamp(it)}" } ?: "") + ".",
                style = MaterialTheme.typography.bodySmall
            )
        }
        if (synced == 0L && attempted == 0L && err.isEmpty() && !running) {
            Text(
                "The first scheduled run can be up to 30 minutes away. " +
                    "Sync now if you don't want to wait.",
                style = MaterialTheme.typography.bodySmall
            )
        }
        if (attempted > 0 && attempted != synced) {
            Text("Last attempt ${ago(attempted)}", style = MaterialTheme.typography.bodySmall)
        }
        if (detail.isNotEmpty()) {
            Text(detail, style = MaterialTheme.typography.labelSmall)
        }
        // A recorded gap outlives the outage that caused it — the data is gone
        // for good, so the admission stays until a later gap replaces it.
        if (gapEnd > 0) {
            Text(
                "Lost to event-log expiry: ${day(gapStart)} → ${day(gapEnd)}. " +
                    "The phone was out of reach longer than Android keeps usage events.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.error
            )
        }
    }

    @Composable
    private fun Updates() {
        val scope = rememberCoroutineScope()
        var available by remember { mutableStateOf<Update.Available?>(null) }
        var checking by remember { mutableStateOf(false) }
        var message by remember { mutableStateOf("") }
        var downloaded by remember { mutableStateOf<File?>(null) }
        var canInstall by remember { mutableStateOf(Update.canInstall(this)) }

        OnResume { canInstall = Update.canInstall(this) }

        // Whatever the last background check found, so the screen is useful
        // before the user taps anything.
        LaunchedEffect(Unit) {
            with(Prefs) {
                val code = this@SettingsActivity.updateCode
                if (code > BuildConfig.VERSION_CODE) {
                    available = Update.Available(
                        this@SettingsActivity.updateVersion, code, "", "/app", "", "unknown"
                    )
                }
            }
        }

        Text("Updates", style = MaterialTheme.typography.titleMedium)
        Text(
            "Installed v${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE})",
            style = MaterialTheme.typography.bodyMedium
        )

        val newer = available?.takeIf { it.newer }
        if (newer != null) {
            Text(
                "Available v${newer.version} (${newer.versionCode})",
                style = MaterialTheme.typography.bodyMedium
            )
        }

        if (message.isNotEmpty()) {
            Text(message, style = MaterialTheme.typography.bodySmall)
        }

        // Installing is a system dialog either way; all this app can do is get
        // the bytes there and be honest that a tap is unavoidable.
        if (newer != null && !canInstall) {
            Button(
                onClick = { startActivity(Update.installPermissionIntent(this@SettingsActivity)) },
                modifier = Modifier.fillMaxWidth()
            ) { Text("Allow installing updates") }
        } else if (downloaded != null) {
            Button(
                onClick = {
                    startActivity(Update.installIntent(this@SettingsActivity, downloaded!!))
                },
                modifier = Modifier.fillMaxWidth()
            ) { Text("Install v${newer?.version ?: ""}") }
        } else if (newer != null) {
            Button(
                onClick = {
                    scope.launch {
                        message = "Downloading…"
                        val f = withContext(Dispatchers.IO) {
                            Update.download(this@SettingsActivity, newer)
                        }
                        downloaded = f
                        message = if (f == null) "Download failed." else "Ready to install."
                    }
                },
                modifier = Modifier.fillMaxWidth()
            ) { Text("Download v${newer.version}") }
        }

        TextButton(
            onClick = {
                scope.launch {
                    checking = true
                    message = "Checking…"
                    val a = withContext(Dispatchers.IO) { Update.check(this@SettingsActivity) }
                    checking = false
                    available = a
                    message = when {
                        a == null -> "Couldn't reach the server."
                        a.newer -> ""
                        else -> "Up to date."
                    }
                }
            },
            enabled = !checking,
            modifier = Modifier.fillMaxWidth()
        ) { Text("Check for updates") }
    }

    /** Runs [block] every time the screen returns to the foreground. */
    @Composable
    private fun OnResume(block: () -> Unit) {
        val owner = LocalLifecycleOwner.current
        DisposableEffect(owner) {
            val obs = LifecycleEventObserver { _, e ->
                if (e == Lifecycle.Event.ON_RESUME) block()
            }
            owner.lifecycle.addObserver(obs)
            onDispose { owner.lifecycle.removeObserver(obs) }
        }
    }
}
