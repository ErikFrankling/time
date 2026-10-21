package se.frankling.time

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class MediaTest {

    @get:Rule
    val tmp = TemporaryFolder()

    @Test
    fun `a span counts as media when most of its minutes had media`() {
        val m = 60_000L
        val samples = setOf(0L, m, 2 * m)
        assertTrue(MediaSamples.mostlyPlaying(samples, 0, 4 * m))
        assertFalse(MediaSamples.mostlyPlaying(samples, 2 * m, 6 * m))
        assertFalse(MediaSamples.mostlyPlaying(emptySet(), 0, m))
    }

    @Test
    fun `only playing minutes are kept, and old ones age out`() {
        val s = MediaSamples(tmp.root.resolve("m.txt"))
        val day = 24 * 3600_000L
        s.record(0, true)
        s.record(60_000, false)
        assertEquals(setOf(0L), s.load())
        s.record(9 * day, true)
        assertEquals(setOf(9 * day), s.load())
    }
}
