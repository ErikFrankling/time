package se.frankling.time

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.net.ConnectException
import java.net.SocketTimeoutException
import java.net.UnknownHostException

/**
 * The two things that turn a working install into a silently broken one: a
 * wrong hash on a downloaded APK, and a failure the user cannot tell apart
 * from any other failure.
 */
class UpdateTest {

    @Test
    fun `hex is lowercase and zero-padded`() {
        assertEquals("000fff", Update.hex(byteArrayOf(0x00, 0x0f, 0xff.toByte())))
    }

    @Test
    fun `off-network failures name the network, not the code`() {
        // The user's fix for all three is the same and is not "read the code".
        for (e in listOf(UnknownHostException("x"), ConnectException("x"))) {
            assertTrue(describe(e).first.contains("LAN or VPN"))
        }
    }

    @Test
    fun `a lan-only rejection is explained as a location, not a refusal`() {
        val (why, detail) = describe(HttpError(403))
        assertTrue(why.contains("LAN"))
        // The raw code stays available, just demoted.
        assertEquals("HTTP 403", detail)
    }

    @Test
    fun `a server fault says there is nothing to do on the phone`() {
        val (why, detail) = describe(HttpError(503))
        assertTrue(why.contains("retrying"))
        assertEquals("HTTP 503", detail)
    }

    @Test
    fun `a timeout is not reported as unreachable`() {
        assertTrue(describe(SocketTimeoutException("x")).first.contains("too long"))
    }
}
