package se.frankling.time

import android.content.Context
import android.media.AudioManager
import java.io.File

/**
 * Whether audio or video is playing, sampled once a minute by [SyncService].
 *
 * Android has no history of media playback an app can read without a
 * notification-listener grant, so the service samples `isMusicActive` (true
 * for any media stream, video included) each minute and keeps the minutes it
 * was true. A sync then marks an app span as `media.playing` when most of it
 * had media on. One cheap system call a minute; no extra permission.
 */
class MediaSamples(private val file: File) {

    fun record(minuteStartMs: Long, playing: Boolean) {
        if (!playing) return
        val keep = (load() + minuteStartMs).filter { it >= minuteStartMs - 8 * 24 * 3600_000L }
        file.writeText(keep.sorted().joinToString("\n"))
    }

    fun load(): Set<Long> = try {
        if (file.exists()) file.readLines().mapNotNull { it.trim().toLongOrNull() }.toSet() else emptySet()
    } catch (_: Exception) {
        emptySet()
    }

    companion object {
        fun of(ctx: Context) = MediaSamples(File(ctx.filesDir, "media-minutes.txt"))

        fun playingNow(ctx: Context): Boolean =
            ctx.getSystemService(AudioManager::class.java)?.isMusicActive == true

        /** More than half of the span's minutes had media playing. */
        fun mostlyPlaying(samples: Set<Long>, startMs: Long, endMs: Long): Boolean {
            val first = startMs / 60_000 * 60_000
            val minutes = generateSequence(first) { it + 60_000 }.takeWhile { it < endMs }.toList()
            if (minutes.isEmpty()) return false
            return minutes.count { it in samples } * 2 > minutes.size
        }
    }
}
