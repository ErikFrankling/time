package se.frankling.time

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** Re-arm after a reboot or an app update, or sync stops silently forever. */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        SyncWorker.schedule(context)
    }
}
