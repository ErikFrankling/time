package se.frankling.time

import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class SpoolTest {

    @get:Rule
    val tmp = TemporaryFolder()

    private fun spool() = Spool(tmp.root.resolve("spool.json"))
    private val now = 1_800_000_000_000L
    private fun span(start: Long, end: Long = start + 60_000) =
        Envelope.appSpan("phone", "com.reader", "Reader", start, end, false)

    @Test
    fun `a backlog comes back oldest first regardless of arrival order`() {
        val out = spool().merge(listOf(span(now - 1000), span(now - 5000), span(now - 3000)), now)
        assertEquals(listOf(now - 5000, now - 3000, now - 1000), out.map { it.startMs })
    }

    @Test
    fun `re-read events replace rather than duplicate`() {
        val s = spool()
        s.merge(listOf(span(now - 5000)), now)
        assertEquals(1, s.merge(listOf(span(now - 5000)), now).size)
    }

    @Test
    fun `only what the server acknowledged is forgotten`() {
        val s = spool()
        val all = s.merge(listOf(span(now - 3000), span(now - 2000)), now)
        assertEquals(listOf(all[1].id), s.ack(listOf(all[0].id)).map { it.id })
        assertEquals(1, Spool(tmp.root.resolve("spool.json")).size())
    }

    @Test
    fun `a backlog outlives the system's seven-day retention but not a month`() {
        val s = spool()
        val day = 24 * 3600_000L
        val out = s.merge(listOf(span(now - 10 * day), span(now - 40 * day)), now)
        assertEquals(listOf(now - 10 * day), out.map { it.startMs })
    }

    @Test
    fun `an unreadable spool does not wedge every future sync`() {
        tmp.root.resolve("spool.json").writeText("not json")
        assertEquals(1, spool().merge(listOf(span(now)), now).size)
    }
}
