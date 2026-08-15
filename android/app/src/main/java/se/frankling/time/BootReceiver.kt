package se.frankling.time

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** Re-arm after a reboot or an app update, or sync stops silently forever. */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        // Both legs. BOOT_COMPLETED and MY_PACKAGE_REPLACED are exempt
        // background-start contexts, so the foreground service is allowed
        // from here even without the battery exemption.
        SyncWorker.schedule(context)
        SyncService.start(context)
    }
}
