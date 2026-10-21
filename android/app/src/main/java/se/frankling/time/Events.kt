package se.frankling.time

import org.json.JSONArray
import org.json.JSONObject
import java.util.UUID

/**
 * One event, in the same shape as `time_core::event::Envelope`.
 *
 * Kept as plain JSON fields rather than a class per event type: the phone
 * sends three kinds (app spans, steps, heartbeats), and the encoder below is
 * generic over attributes exactly like the Rust one. The contract between the
 * two is the fixture in `fixtures/otlp/phone-batch.json`, which the Rust
 * server's tests decode and this app's tests reproduce.
 */
data class Envelope(
    val id: String,
    val name: String,
    val startMs: Long,
    val endMs: Long,
    /** Attribute key to value: String, Long, Int, Double, Boolean or List<String>. */
    val fields: Map<String, Any>,
) {
    fun toJson(): JSONObject = JSONObject().apply {
        put("id", id)
        put("name", name)
        put("start", startMs)
        put("end", endMs)
        put("fields", JSONObject(fields.mapValues { (_, v) -> if (v is List<*>) JSONArray(v) else v }))
    }

    companion object {
        fun fromJson(o: JSONObject): Envelope {
            val f = o.getJSONObject("fields")
            val fields = f.keys().asSequence().associateWith { k ->
                when (val v = f.get(k)) {
                    is JSONArray -> (0 until v.length()).map { v.getString(it) }
                    else -> v
                }
            }
            return Envelope(o.getString("id"), o.getString("name"), o.getLong("start"), o.getLong("end"), fields)
        }

        /**
         * An id derived from what the event is, so re-reading the same stretch
         * of the usage log yields the same id and the server stores it once.
         */
        fun stableId(device: String, name: String, startMs: Long, endMs: Long): String =
            UUID.nameUUIDFromBytes("$device|${Otlp.SERVICE}|$startMs|$endMs|$name".toByteArray()).toString()

        fun appSpan(device: String, pkg: String, label: String, startMs: Long, endMs: Long, media: Boolean) =
            Envelope(
                stableId(device, "time.phone.app", startMs, endMs), "time.phone.app", startMs, endMs,
                mapOf("app.package" to pkg, "app.label" to label, "media.playing" to media),
            )

        fun steps(device: String, startMs: Long, endMs: Long, steps: Long, meters: Double) =
            Envelope(
                stableId(device, "time.health.steps", startMs, endMs), "time.health.steps", startMs, endMs,
                mapOf("steps" to steps, "distance.meters" to meters),
            )

        fun heartbeat(nowMs: Long, pending: Int, version: String, coveredUntil: Long, missing: List<String>) =
            Envelope(
                UUID.randomUUID().toString(), "time.heartbeat", nowMs, nowMs,
                buildMap {
                    put("spool.pending", pending.toLong())
                    put("app.version", version)
                    put("covered.until", coveredUntil)
                    if (missing.isNotEmpty()) put("missing", missing)
                },
            )
    }
}

/** OTLP/HTTP JSON, the subset Time uses. Mirrors `time_core::otlp::encode`. */
object Otlp {
    const val SERVICE = "time-android"
    const val NAMESPACE = "time"

    fun encode(device: String, version: String, events: List<Envelope>): JSONObject {
        val records = JSONArray()
        for (e in events) records.put(record(e))
        val resource = JSONObject().put(
            "attributes", JSONArray()
                .put(kv("service.name", SERVICE))
                .put(kv("service.namespace", NAMESPACE))
                .put(kv("service.version", version))
                .put(kv("host.name", device))
        )
        val scope = JSONObject()
            .put("scope", JSONObject().put("name", "time").put("version", version))
            .put("logRecords", records)
        return JSONObject().put(
            "resourceLogs",
            JSONArray().put(JSONObject().put("resource", resource).put("scopeLogs", JSONArray().put(scope)))
        )
    }

    private fun record(e: Envelope): JSONObject {
        val attrs = JSONArray()
            .put(kv("event.name", e.name))
            .put(kv("event.id", e.id))
            .put(kv("time.start", e.startMs))
            .put(kv("time.end", e.endMs))
        for ((k, v) in e.fields.toSortedMap()) attrs.put(kv(k, v))
        return JSONObject()
            .put("timeUnixNano", (e.startMs * 1_000_000).toString())
            .put("eventName", e.name)
            .put("attributes", attrs)
    }

    fun kv(key: String, value: Any): JSONObject = JSONObject().put("key", key).put("value", any(value))

    /** OTLP's AnyValue. Integers are decimal strings, per the protobuf JSON mapping. */
    private fun any(v: Any): JSONObject = when (v) {
        is String -> JSONObject().put("stringValue", v)
        is Boolean -> JSONObject().put("boolValue", v)
        is Int -> JSONObject().put("intValue", v.toString())
        is Long -> JSONObject().put("intValue", v.toString())
        is Double -> JSONObject().put("doubleValue", v)
        is Float -> JSONObject().put("doubleValue", v.toDouble())
        is List<*> -> JSONObject().put(
            "arrayValue",
            JSONObject().put("values", JSONArray(v.map { any(it ?: "") }))
        )
        else -> JSONObject().put("stringValue", v.toString())
    }
}
