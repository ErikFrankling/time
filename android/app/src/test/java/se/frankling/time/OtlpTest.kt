package se.frankling.time

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

/**
 * The contract with the server: this encoder must produce exactly
 * `fixtures/otlp/phone-batch.json`, which the Rust side's
 * `otlp::phone_contract` test decodes into the expected events.
 */
class OtlpTest {

    private fun fixture(): JSONObject {
        // Unit tests run from android/app.
        val f = listOf("../../fixtures/otlp/phone-batch.json", "../fixtures/otlp/phone-batch.json")
            .map(::File).first { it.exists() }
        return JSONObject(f.readText())
    }

    @Test
    fun `the encoder reproduces the shared fixture`() {
        val events = listOf(
            Envelope(
                "11111111-1111-3111-8111-111111111111", "time.phone.app", 1792566000000, 1792566600000,
                mapOf("app.package" to "com.twitter.android", "app.label" to "X", "media.playing" to false),
            ),
            Envelope(
                "22222222-2222-3222-8222-222222222222", "time.health.steps", 1792566000000, 1792569600000,
                mapOf("steps" to 1200L, "distance.meters" to 850.5),
            ),
            Envelope(
                "33333333-3333-4333-8333-333333333333", "time.heartbeat", 1792566900000, 1792566900000,
                mapOf(
                    "spool.pending" to 0L, "app.version" to "test", "covered.until" to 1792566900000L,
                    "missing" to listOf("health: steps/distance permission not granted"),
                ),
            ),
        )
        val got = Otlp.encode("phone", "test", events)
        val want = fixture()
        assertTrue("encoded:\n${got.toString(2)}\nfixture:\n${want.toString(2)}", got.similar(want))
    }

    @Test
    fun `re-reading the same span gives the same id`() {
        val a = Envelope.appSpan("phone", "com.x", "X", 1, 2, false)
        val b = Envelope.appSpan("phone", "com.x", "X", 1, 2, true)
        assertEquals(a.id, b.id)
        assertNotEquals(a.id, Envelope.appSpan("phone", "com.x", "X", 1, 3, false).id)
    }

    @Test
    fun `envelopes survive the spool's JSON`() {
        val e = Envelope.heartbeat(5, 0, "v", 4, listOf("a", "b"))
        assertEquals(e, Envelope.fromJson(JSONObject(e.toJson().toString())).let {
            // org.json reads small longs back as Int; compare through the encoder.
            Envelope(it.id, it.name, it.startMs, it.endMs, it.fields.mapValues { (_, v) -> if (v is Int) v.toLong() else v })
        })
    }
}
