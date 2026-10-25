package se.frankling.time

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class PhoneAudioTest {

    @get:Rule
    val tmp = TemporaryFolder()

    @Test
    fun `only playing media with the volume up is audible`() {
        assertTrue(PhoneAudio.audible(musicActive = true, volume = 7, muted = false))
        assertFalse("paused", PhoneAudio.audible(musicActive = false, volume = 7, muted = false))
        assertFalse("volume at zero", PhoneAudio.audible(musicActive = true, volume = 0, muted = false))
        assertFalse("muted", PhoneAudio.audible(musicActive = true, volume = 7, muted = true))
    }

    @Test
    fun `apps are named as the service so phone and computer add up`() {
        assertEquals("spotify", PhoneAudio.source("com.spotify.music", "Spotify"))
        assertEquals("youtube.com", PhoneAudio.source("com.google.android.youtube", "YouTube"))
        assertEquals("pocket casts", PhoneAudio.source("au.com.shiftyjelly.pocketcasts", "Pocket Casts"))
        assertEquals("phone (other)", PhoneAudio.source("", ""))
    }

    @Test
    fun `samples become one listening event per minute and track`() {
        val s = AudioSamples(tmp.root.resolve("a.jsonl"))
        val song = PhoneAudio.Heard("com.spotify.music", "Spotify", "Boulevard of Broken Dreams", "Green Day")
        val video = PhoneAudio.Heard("com.google.android.youtube", "YouTube", "", "")
        repeat(4) { s.add(60_000L, 15, listOf(song)) }
        s.add(120_000L, 15, listOf(song))
        s.add(120_000L, 15, listOf(video))
        val events = AudioSamples.toEvents("phone", s.read()).sortedBy { it.startMs }
        assertEquals(3, events.size)
        val first = events[0]
        assertEquals("time.audio.listening", first.name)
        assertEquals("spotify", first.fields["audio.source"])
        assertEquals("Green Day", first.fields["media.artist"])
        assertEquals(60L, first.fields["audible.seconds"])
        val (a, b) = events.drop(1)
        assertEquals(15L, a.fields["audible.seconds"])
        assertNotEquals("two apps in one minute are two events", a.id, b.id)
        assertEquals(events.map { it.id }, AudioSamples.toEvents("phone", s.read()).sortedBy { it.startMs }.map { it.id })
    }
}
