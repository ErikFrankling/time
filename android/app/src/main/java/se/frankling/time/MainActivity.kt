package se.frankling.time

import android.net.Uri
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.PowerManager
import android.os.Bundle
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.platform.LocalLifecycleOwner
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
import se.frankling.time.Prefs.device
import se.frankling.time.Prefs.lastAttempt
import se.frankling.time.Prefs.lastError
import se.frankling.time.Prefs.lastErrorDetail
import se.frankling.time.Prefs.lastSync
import se.frankling.time.Prefs.server

class MainActivity : ComponentActivity() {

    private val askNotifications =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        SyncWorker.schedule(this)

        // The update prompt is the only notification this app posts, and the
        // grant is cheap to ask for once at first launch.
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            askNotifications.launch(android.Manifest.permission.POST_NOTIFICATIONS)
        }

        // The periodic work has a 30-minute period, so
        // without this the first half hour after granting access looks exactly
        // like a broken app.
        if (lastSync == 0L && Usage.hasAccess(this)) SyncWorker.once(this)

        setContent { MaterialTheme { Screen() } }
    }

    @Composable
    private fun Screen() {
        var srv by remember { mutableStateOf(server) }
        var dev by remember { mutableStateOf(device) }

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

        val running = once.any { it.state == WorkInfo.State.RUNNING } ||
            periodic.any { it.state == WorkInfo.State.RUNNING }
        val nextRun = periodic.firstOrNull()?.nextScheduleTimeMillis
            ?.takeIf { it in 1..<Long.MAX_VALUE }

        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(24.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp)
        ) {
            Text("time", style = MaterialTheme.typography.headlineMedium)
            Text(
                "Reports which app is in the foreground, once every 30 minutes. " +
                    "No screenshots — Android does not allow them silently.",
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

            SyncStatus(running, granted, synced, attempted, err, detail, nextRun)

            // Optional, and the single biggest thing available for keeping the
            // schedule honest. An app opened once a week decays to the RARE
            // standby bucket, where the whole quota is three job sessions a day
            // -- so a thirty-minute period quietly becomes eight-hourly. The
            // exemption pins the app to EXEMPTED, which stops the decay and
            // lets jobs run through Doze. One dialog, no notification, no
            // service; strictly a better trade than a foreground service.
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
                    "Optional. Without it Android throttles the sync to roughly three " +
                        "times a day once the app has gone a couple of days unopened.",
                    style = MaterialTheme.typography.bodySmall
                )
            }

            Button(
                onClick = { SyncWorker.once(this@MainActivity) },
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

    @Composable
    private fun SyncStatus(
        running: Boolean,
        granted: Boolean,
        synced: Long,
        attempted: Long,
        err: String,
        detail: String,
        nextRun: Long?,
    ) {
        val stamp = { t: Long ->
            SimpleDateFormat("HH:mm", Locale.getDefault()).format(Date(t))
        }

        val headline = when {
            running -> "Syncing…"
            !granted -> "Paused — usage access is off"
            err.isNotEmpty() -> err
            synced == 0L && attempted == 0L -> "Waiting for the first sync"
            synced == 0L -> "No data reported yet"
            else -> "Working — last sync ${stamp(synced)}"
        }

        Text("Status", style = MaterialTheme.typography.titleMedium)
        Text(headline, style = MaterialTheme.typography.bodyLarge)

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
            Text("Last attempt ${stamp(attempted)}", style = MaterialTheme.typography.bodySmall)
        }
        if (detail.isNotEmpty()) {
            Text(detail, style = MaterialTheme.typography.labelSmall)
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
                val code = this@MainActivity.updateCode
                if (code > BuildConfig.VERSION_CODE) {
                    available = Update.Available(
                        this@MainActivity.updateVersion, code, "", "/app", "", "unknown"
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
                onClick = { startActivity(Update.installPermissionIntent(this@MainActivity)) },
                modifier = Modifier.fillMaxWidth()
            ) { Text("Allow installing updates") }
        } else if (downloaded != null) {
            Button(
                onClick = {
                    startActivity(Update.installIntent(this@MainActivity, downloaded!!))
                },
                modifier = Modifier.fillMaxWidth()
            ) { Text("Install v${newer?.version ?: ""}") }
        } else if (newer != null) {
            Button(
                onClick = {
                    scope.launch {
                        message = "Downloading…"
                        val f = withContext(Dispatchers.IO) {
                            Update.download(this@MainActivity, newer)
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
                    val a = withContext(Dispatchers.IO) { Update.check(this@MainActivity) }
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
