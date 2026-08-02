package se.frankling.time

import android.app.usage.UsageEvents.Event.ACTIVITY_PAUSED
import android.app.usage.UsageEvents.Event.ACTIVITY_RESUMED
import android.app.usage.UsageEvents.Event.SCREEN_NON_INTERACTIVE
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * The three ways this state machine invents time that never happened. Each one
 * is silent in production: the numbers are simply wrong and look plausible.
 */
class UsageTest {

    private val t0 = 1_700_000_000_000L
    private fun m(n: Long) = t0 + n * 60_000

    @Test
    fun `screen off closes the open session`() {
        val s = Usage.sessions(
            listOf(
                Ev(ACTIVITY_RESUMED, "com.reader", m(0)),
                Ev(SCREEN_NON_INTERACTIVE, "com.reader", m(10)),
                // Eight hours later, without a PAUSE ever arriving.
                Ev(ACTIVITY_RESUMED, "com.clock", m(480)),
                Ev(ACTIVITY_PAUSED, "com.clock", m(482)),
            )
        )
        assertEquals(listOf(Session("com.reader", m(0), m(10)), Session("com.clock", m(480), m(482))), s)
    }

    @Test
    fun `another app resuming closes the previous one`() {
        // No PAUSE for the first app at all -- the common case, since PAUSE is
        // frequently late or dropped entirely.
        val s = Usage.sessions(
            listOf(
                Ev(ACTIVITY_RESUMED, "com.a", m(0)),
                Ev(ACTIVITY_RESUMED, "com.b", m(5)),
                Ev(ACTIVITY_PAUSED, "com.b", m(9)),
            )
        )
        assertEquals(listOf(Session("com.a", m(0), m(5)), Session("com.b", m(5), m(9))), s)
        // Nine minutes of wall clock, nine minutes accounted for. Pair matching
        // would have produced fourteen.
        assertEquals(9 * 60_000L, s.sumOf { it.end - it.start })
    }

    @Test
    fun `the trailing open session is not emitted`() {
        val s = Usage.sessions(
            listOf(
                Ev(ACTIVITY_RESUMED, "com.a", m(0)),
                Ev(ACTIVITY_PAUSED, "com.a", m(3)),
                Ev(ACTIVITY_RESUMED, "com.b", m(3)),
            )
        )
        assertEquals(listOf(Session("com.a", m(0), m(3))), s)
    }

    @Test
    fun `re-resume of the same app keeps the earlier start`() {
        val s = Usage.sessions(
            listOf(
                Ev(ACTIVITY_RESUMED, "com.a", m(0)),
                Ev(ACTIVITY_RESUMED, "com.a", m(1)),
                Ev(ACTIVITY_PAUSED, "com.a", m(4)),
            )
        )
        assertEquals(listOf(Session("com.a", m(0), m(4))), s)
    }

    @Test
    fun `implausible durations are dropped`() {
        val s = Usage.sessions(
            listOf(
                // A sub-second blip from swiping through a launcher.
                Ev(ACTIVITY_RESUMED, "com.blip", t0),
                Ev(ACTIVITY_PAUSED, "com.blip", t0 + 200),
                // Five hours means an event went missing, not five hours of use.
                Ev(ACTIVITY_RESUMED, "com.stuck", m(60)),
                Ev(ACTIVITY_PAUSED, "com.stuck", m(360)),
            )
        )
        assertEquals(emptyList<Session>(), s)
    }
}
