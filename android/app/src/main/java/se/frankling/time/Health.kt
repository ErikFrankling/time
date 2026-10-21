package se.frankling.time

import android.content.Context
import androidx.health.connect.client.HealthConnectClient
import androidx.health.connect.client.HealthConnectFeatures
import androidx.health.connect.client.permission.HealthPermission
import androidx.health.connect.client.records.DistanceRecord
import androidx.health.connect.client.records.StepsRecord
import androidx.health.connect.client.request.AggregateGroupByDurationRequest
import androidx.health.connect.client.time.TimeRangeFilter
import kotlinx.coroutines.runBlocking
import java.time.Duration
import java.time.Instant

/**
 * Steps and distance from Health Connect, per hour.
 *
 * Health Connect is filled by what the phone already runs (Fit, the
 * pedometer, a watch), so reading it costs no battery and needs no GPS --
 * the app never asks for location at all. Hours are read once they are
 * settled (two hours old), because a watch syncing late would otherwise
 * change an hour after it was sent, and an event id is forever.
 */
object Health {

    val PERMISSIONS = setOf(
        HealthPermission.getReadPermission(StepsRecord::class),
        HealthPermission.getReadPermission(DistanceRecord::class),
    )
    const val BACKGROUND = HealthPermission.PERMISSION_READ_HEALTH_DATA_IN_BACKGROUND

    private const val HOUR = 3600_000L
    const val SETTLE_MS = 2 * HOUR

    fun available(ctx: Context): Boolean =
        HealthConnectClient.getSdkStatus(ctx) == HealthConnectClient.SDK_AVAILABLE

    /** What is missing, in words the status screen can show. Empty when all is well. */
    fun missing(ctx: Context): List<String> {
        if (!available(ctx)) return listOf("health: Health Connect is not available on this phone")
        return try {
            val client = HealthConnectClient.getOrCreate(ctx)
            val granted = runBlocking { client.permissionController.getGrantedPermissions() }
            buildList {
                if (!granted.containsAll(PERMISSIONS)) add("health: steps/distance permission not granted")
                if (backgroundSupported(client) && BACKGROUND !in granted) {
                    add("health: background read not granted (steps only sync while the app is open)")
                }
            }
        } catch (e: Exception) {
            listOf("health: ${e.message ?: e.javaClass.simpleName}")
        }
    }

    private fun backgroundSupported(client: HealthConnectClient): Boolean =
        client.features.getFeatureStatus(HealthConnectFeatures.FEATURE_READ_HEALTH_DATA_IN_BACKGROUND) ==
            HealthConnectFeatures.FEATURE_STATUS_AVAILABLE

    /** Settled hours in [fromMs, now - SETTLE_MS), as events. */
    fun read(ctx: Context, device: String, fromMs: Long, nowMs: Long): Pair<List<Envelope>, Long>? {
        if (!available(ctx)) return null
        val client = HealthConnectClient.getOrCreate(ctx)
        val start = fromMs / HOUR * HOUR
        val end = (nowMs - SETTLE_MS) / HOUR * HOUR
        if (end <= start) return emptyList<Envelope>() to start
        return runBlocking {
            val granted = client.permissionController.getGrantedPermissions()
            if (!granted.containsAll(PERMISSIONS)) return@runBlocking null
            val groups = client.aggregateGroupByDuration(
                AggregateGroupByDurationRequest(
                    metrics = setOf(StepsRecord.COUNT_TOTAL, DistanceRecord.DISTANCE_TOTAL),
                    timeRangeFilter = TimeRangeFilter.between(Instant.ofEpochMilli(start), Instant.ofEpochMilli(end)),
                    timeRangeSlicer = Duration.ofHours(1),
                )
            )
            val events = groups.mapNotNull { g ->
                val steps = g.result[StepsRecord.COUNT_TOTAL] ?: 0L
                val meters = g.result[DistanceRecord.DISTANCE_TOTAL]?.inMeters ?: 0.0
                if (steps == 0L && meters == 0.0) null
                else Envelope.steps(device, g.startTime.toEpochMilli(), g.endTime.toEpochMilli(), steps, meters)
            }
            events to end
        }
    }
}
