package se.frankling.time

import android.app.usage.UsageEvents
import android.app.usage.UsageStatsManager
import android.content.ComponentName
import android.content.Context
import android.media.AudioManager
import android.media.MediaMetadata
import android.media.session.MediaController
import android.media.session.MediaSessionManager
import android.media.session.PlaybackState
import android.provider.Settings
import android.service.notification.NotificationListenerService
import org.json.JSONObject
import java.io.File

/**
 * What is audible on the phone, sampled every [EVERY_MS] by [SyncService],
 * sent as the same `time.audio.listening` events the computers send.
 *
 * Audible means: the audio system has a media stream actually playing
 * (`isMusicActive`, false the moment playback pauses), and the media volume
 * is neither zero nor muted. Which app and track come from the active media
 * sessions, readable once notification access is granted to [MediaListener]
 * (the listener itself does nothing; Android only hands media sessions to an
 * app holding that grant). Without it, the foreground app is named instead,
 * which covers YouTube on screen but not Spotify in a pocket.
 */
object PhoneAudio {
    const val EVERY_MS = 15_000L

    data class Heard(val pkg: String, val label: String, val title: String, val artist: String)

    fun hasSessionAccess(ctx: Context): Boolean =
        Settings.Secure.getString(ctx.contentResolver, "enabled_notification_listeners")
            ?.contains(ComponentName(ctx, MediaListener::class.java).flattenToString()) == true

    /** Audible right now, or empty. */
    fun sample(ctx: Context): List<Heard> {
        val am = ctx.getSystemService(AudioManager::class.java) ?: return emptyList()
        if (!audible(am.isMusicActive, am.getStreamVolume(AudioManager.STREAM_MUSIC), am.isStreamMute(AudioManager.STREAM_MUSIC))) {
            return emptyList()
        }
        val playing = sessions(ctx)
        if (playing.isNotEmpty()) return playing
        val pkg = foreground(ctx) ?: return listOf(Heard("", "", "", ""))
        return listOf(Heard(pkg, Usage.label(ctx, pkg), "", ""))
    }

    fun audible(musicActive: Boolean, volume: Int, muted: Boolean) = musicActive && volume > 0 && !muted

    private fun sessions(ctx: Context): List<Heard> {
        if (!hasSessionAccess(ctx)) return emptyList()
        val msm = ctx.getSystemService(MediaSessionManager::class.java) ?: return emptyList()
        val controllers = try {
            msm.getActiveSessions(ComponentName(ctx, MediaListener::class.java))
        } catch (_: SecurityException) {
            return emptyList()
        }
        return controllers.filter { playingLocally(it) }.map { c ->
            val m = c.metadata
            Heard(
                c.packageName,
                Usage.label(ctx, c.packageName),
                m?.getString(MediaMetadata.METADATA_KEY_TITLE) ?: m?.getString(MediaMetadata.METADATA_KEY_DISPLAY_TITLE) ?: "",
                m?.getString(MediaMetadata.METADATA_KEY_ARTIST) ?: "",
            )
        }
    }

    /** Playing, on this phone (not cast to a TV), with its own volume up. */
    private fun playingLocally(c: MediaController): Boolean {
        if (c.playbackState?.state != PlaybackState.STATE_PLAYING) return false
        val info = c.playbackInfo ?: return true
        return info.playbackType == MediaController.PlaybackInfo.PLAYBACK_TYPE_LOCAL
    }

    /** The app most recently brought to the front in the last hour. */
    private fun foreground(ctx: Context): String? {
        val usm = ctx.getSystemService(UsageStatsManager::class.java) ?: return null
        val now = System.currentTimeMillis()
        val events = usm.queryEvents(now - 3600_000L, now) ?: return null
        var last: String? = null
        val e = UsageEvents.Event()
        while (events.hasNextEvent()) {
            events.getNextEvent(e)
            if (e.eventType == UsageEvents.Event.ACTIVITY_RESUMED) last = e.packageName
        }
        return last
    }

    /** `audio.source` for an app: the service it is, so phone and computer add up. */
    fun source(pkg: String, label: String): String = when (pkg) {
        "com.spotify.music" -> "spotify"
        "com.google.android.youtube", "app.revanced.android.youtube", "app.rvx.android.youtube" -> "youtube.com"
        "com.google.android.apps.youtube.music" -> "music.youtube.com"
        "com.netflix.mediaclient" -> "netflix.com"
        "tv.twitch.android.app" -> "twitch.tv"
        "com.twitter.android" -> "x.com"
        "com.soundcloud.android" -> "soundcloud.com"
        "" -> "phone (other)"
        else -> label.ifBlank { pkg }.lowercase()
    }
}

/**
 * Exists only to hold notification access, which is what lets [PhoneAudio]
 * read media sessions. Notifications themselves are never read.
 */
class MediaListener : NotificationListenerService()

/** Audible samples on disk until the next sync turns them into events. */
class AudioSamples(private val file: File) {

    fun add(minuteStart: Long, seconds: Int, heard: List<PhoneAudio.Heard>) {
        val lines = heard.joinToString("") { h ->
            JSONObject()
                .put("ts", minuteStart).put("secs", seconds)
                .put("pkg", h.pkg).put("label", h.label).put("title", h.title).put("artist", h.artist)
                .toString() + "\n"
        }
        if (lines.isNotEmpty()) file.appendText(lines)
    }

    fun read(): List<JSONObject> = try {
        if (!file.exists()) emptyList() else file.readLines().mapNotNull { runCatching { JSONObject(it) }.getOrNull() }
    } catch (_: Exception) {
        emptyList()
    }

    fun clear() {
        file.delete()
    }

    companion object {
        fun of(ctx: Context) = AudioSamples(File(ctx.filesDir, "audio-samples.jsonl"))

        /**
         * One `time.audio.listening` event per minute and app (and track), the
         * samples' seconds added up. Stable ids, so a resent minute is stored
         * once; the key includes the app, since the id is otherwise per minute.
         */
        fun toEvents(device: String, samples: List<JSONObject>): List<Envelope> =
            samples.groupBy { Triple(it.getLong("ts"), it.optString("pkg"), it.optString("title")) }
                .map { (key, group) ->
                    val (ts, pkg, title) = key
                    val first = group.first()
                    val label = first.optString("label")
                    val secs = group.sumOf { it.optInt("secs") }.coerceAtMost(60)
                    val fields = buildMap<String, Any> {
                        put("audio.app", label.ifBlank { pkg.ifBlank { "phone" } })
                        put("audio.source", PhoneAudio.source(pkg, label))
                        if (title.isNotBlank()) put("media.title", title)
                        first.optString("artist").takeIf { it.isNotBlank() }?.let { put("media.artist", it) }
                        put("audible.seconds", secs.toLong())
                        put("mpris.status", "Playing")
                    }
                    Envelope(
                        Envelope.stableId(device, "time.audio.listening|$pkg|$title", ts, ts + 60_000),
                        "time.audio.listening", ts, ts + 60_000, fields,
                    )
                }
    }
}
