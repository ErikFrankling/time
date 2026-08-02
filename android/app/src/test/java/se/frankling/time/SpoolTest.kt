package se.frankling.time

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

/**
 * The spool exists because the system event log forgets after a week. What has
 * to hold: a minute survives the process, arrives in order, and is only
 * forgotten once the server has said it has it.
 */
class SpoolTest {

    @get:Rule
    val tmp = TemporaryFolder()

    private fun spool() = Spool(tmp.root.resolve("spool.json"))

    private fun frame(ts: Long, window: String = "Reader — com.reader") =
        JSONObject().apply {
            put("ts", ts)
            put("device", "phone")
            put("window", window)
        }

    private fun tsOf(fs: List<JSONObject>) = fs.map { it.getLong("ts") }

    @Test
    fun `a backlog comes back oldest first regardless of arrival order`() {
        val s = spool()
        s.merge(listOf(frame(180), frame(60)), 1000)
        val all = s.merge(listOf(frame(120), frame(0)), 1000)
        assertEquals(listOf(0L, 60L, 120L, 180L), tsOf(all))
    }

    @Test
    fun `a minute written by one run is there for the next`() {
        spool().merge(listOf(frame(60), frame(120)), 1000)
        // A fresh Spool over the same file is what a process restart looks like.
        assertEquals(listOf(60L, 120L), tsOf(spool().read()))
    }

    @Test
    fun `only what the server acknowledged is forgotten`() {
        val s = spool()
        s.merge((0L until 5L).map { frame(it * 60) }, 1000)
        // Two chunks accepted, the third never sent.
        s.ack(listOf(0L, 60L))
        assertEquals(listOf(120L, 180L, 240L), tsOf(s.read()))
        assertEquals(listOf(120L, 180L, 240L), tsOf(spool().read()))
    }

    @Test
    fun `the overlap between runs does not duplicate a minute`() {
        val s = spool()
        s.merge(listOf(frame(60), frame(120)), 1000)
        // Runs deliberately re-read the last two minutes; the later read saw
        // more of the minute, so it wins.
        val all = s.merge(listOf(frame(120, "Maps — com.maps"), frame(180)), 1000)
        assertEquals(listOf(60L, 120L, 180L), tsOf(all))
        assertEquals("Maps — com.maps", all[1].getString("window"))
    }

    @Test
    fun `a backlog outlives the systems seven-day retention`() {
        val day = 24 * 3600L
        val s = spool()
        s.merge(listOf(frame(0), frame(60)), 0)
        // Ten days later the event log has long since dropped these. The spool
        // has not, which is the entire point of it.
        assertEquals(listOf(0L, 60L), tsOf(s.merge(emptyList(), 10 * day)))
        // Past a month they go, and the oldest goes first.
        assertEquals(listOf(60L), tsOf(s.merge(emptyList(), 30 * day + 30)))
    }

    @Test
    fun `an unreadable spool does not wedge every future sync`() {
        tmp.root.resolve("spool.json").writeText("{ not json")
        val s = spool()
        assertTrue(s.read().isEmpty())
        assertEquals(listOf(60L), tsOf(s.merge(listOf(frame(60)), 1000)))
    }
}
