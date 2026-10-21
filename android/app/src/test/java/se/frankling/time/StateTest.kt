package se.frankling.time

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class StateTest {

    @get:Rule
    val tmp = TemporaryFolder()

    @Test
    fun `samples become one stable event per minute with the server's field names`() {
        val s = StateSamples(tmp.root.resolve("s.jsonl"))
        s.add(JSONObject().put("ts", 60_000L).put("charging", true).put("locked", false).put("interactive", true).put("battery", 80).put("gateway", "192.168.50.1"))
        s.add(JSONObject().put("ts", 120_000L).put("charging", false).put("locked", true).put("interactive", false).put("ssid", "eduroam"))
        val events = StateSamples.toEvents("phone", s.read())
        assertEquals(2, events.size)
        assertEquals("time.phone.state", events[0].name)
        assertEquals(true, events[0].fields["charging"])
        assertEquals(80L, events[0].fields["battery.percent"])
        assertEquals("192.168.50.1", events[0].fields["network.gateway"])
        assertEquals("eduroam", events[1].fields["wifi.ssid"])
        assertFalse(events[1].fields.containsKey("network.gateway"))
        assertEquals(events[0].id, StateSamples.toEvents("phone", s.read())[0].id)
        s.clear()
        assertEquals(0, s.read().size)
    }
}
