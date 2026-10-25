package se.frankling.time

import android.app.Activity
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.media.MediaMetadata
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.os.Bundle
import kotlin.concurrent.thread
import kotlin.math.PI
import kotlin.math.sin

/**
 * Plays a 440 Hz tone as media for `seconds` (default 50), with a media
 * session saying "Smoke tone" by "Time" is playing. Only in debug builds,
 * for `android/scripts/emulator-smoke.sh`.
 */
class TonePlayerActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val seconds = intent.getIntExtra("seconds", 50)
        val session = MediaSession(this, "smoke").apply {
            setMetadata(
                MediaMetadata.Builder()
                    .putString(MediaMetadata.METADATA_KEY_TITLE, "Smoke tone")
                    .putString(MediaMetadata.METADATA_KEY_ARTIST, "Time")
                    .build()
            )
            setPlaybackState(PlaybackState.Builder().setState(PlaybackState.STATE_PLAYING, 0, 1f).build())
            isActive = true
        }
        thread {
            val rate = 44_100
            val track = AudioTrack.Builder()
                .setAudioAttributes(
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                        .build()
                )
                .setAudioFormat(
                    AudioFormat.Builder()
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                        .setSampleRate(rate)
                        .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
                        .build()
                )
                .setTransferMode(AudioTrack.MODE_STREAM)
                .build()
            track.play()
            val buf = ShortArray(rate / 10)
            var n = 0L
            val end = System.currentTimeMillis() + seconds * 1000L
            while (System.currentTimeMillis() < end) {
                for (i in buf.indices) {
                    buf[i] = (sin(2 * PI * 440 * n++ / rate) * 8000).toInt().toShort()
                }
                track.write(buf, 0, buf.size)
            }
            track.stop()
            track.release()
            session.setPlaybackState(PlaybackState.Builder().setState(PlaybackState.STATE_STOPPED, 0, 0f).build())
            session.release()
        }
    }
}
