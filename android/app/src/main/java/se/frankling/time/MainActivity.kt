package se.frankling.time

import android.content.Intent
import android.os.Bundle
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.work.WorkManager
import androidx.work.OneTimeWorkRequestBuilder
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import se.frankling.time.Prefs.device
import se.frankling.time.Prefs.lastError
import se.frankling.time.Prefs.lastSync
import se.frankling.time.Prefs.server

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        SyncWorker.schedule(this)
        setContent { MaterialTheme { Screen() } }
    }

    @Composable
    private fun Screen() {
        var srv by remember { mutableStateOf(server) }
        var dev by remember { mutableStateOf(device) }
        var tick by remember { mutableIntStateOf(0) }
        val granted = remember(tick) { Usage.hasAccess(this) }
        val synced = remember(tick) { lastSync }
        val err = remember(tick) { lastError }

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
                    onClick = {
                        startActivity(Intent(Settings.ACTION_USAGE_ACCESS_SETTINGS))
                    },
                    modifier = Modifier.fillMaxWidth()
                ) { Text("Grant usage access") }
            }

            // An OEM battery-killer shows up as sync quietly stopping. Surfacing
            // the last success makes that visible in seconds rather than as a
            // month-long hole discovered later.
            Text(
                "Last sync: " + if (synced == 0L) "never" else
                    SimpleDateFormat("yyyy-MM-dd HH:mm", Locale.getDefault()).format(Date(synced)),
                style = MaterialTheme.typography.bodyMedium
            )
            if (err.isNotEmpty()) {
                Text("Last error: $err", style = MaterialTheme.typography.bodySmall)
            }

            Button(
                onClick = {
                    WorkManager.getInstance(this@MainActivity)
                        .enqueue(OneTimeWorkRequestBuilder<SyncWorker>().build())
                    tick++
                },
                modifier = Modifier.fillMaxWidth()
            ) { Text("Sync now") }

            TextButton(onClick = { tick++ }, modifier = Modifier.fillMaxWidth()) {
                Text("Refresh status")
            }

            Text(
                "v${BuildConfig.VERSION_NAME}",
                style = MaterialTheme.typography.labelSmall
            )
        }
    }
}
