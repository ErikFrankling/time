package se.frankling.time

import android.annotation.SuppressLint
import android.content.Intent
import android.graphics.Color
import android.os.Bundle
import android.view.Gravity
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import se.frankling.time.Prefs.lastSync
import se.frankling.time.Prefs.server

/**
 * The app is the website.
 *
 * Day view, status screen and everything after them are the server's pages,
 * shown here as they are in a browser -- one UI, not two kept in step. The
 * only native screen is [SettingsActivity], for what only the phone can do
 * (grant usage access and Health Connect, set the gateway and token). This
 * screen adds a thin bar with a way there, and an honest page when the
 * server cannot be reached.
 */
class MainActivity : ComponentActivity() {

    private companion object {
        val DARK = Color.parseColor("#141413")
        val LIGHT = Color.parseColor("#ECEBE6")
        val MUTED = Color.parseColor("#9D9C95")
        val BAR = Color.parseColor("#1D1D1B")
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    private lateinit var web: WebView
    private lateinit var error: TextView

    @SuppressLint("SetJavaScriptEnabled")
    override fun onCreate(savedInstanceState: Bundle?) {
        // Android 15 draws every app edge to edge. Say so explicitly, with light
        // icons on the dark bars, and pad the content by the bars and the
        // display cutout below so nothing sits under the clock or the
        // gesture bar.
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
        )
        super.onCreate(savedInstanceState)
        // Both legs on every open: the worker in case its spec died, the
        // service in case a force-stop took it down.
        SyncWorker.schedule(this)
        SyncService.start(this)
        if (lastSync < System.currentTimeMillis() - 20 * 60_000L) SyncWorker.once(this)

        // The native bar: only the way to the phone's own settings. The site
        // below has its own header with the page names.
        val bar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setBackgroundColor(BAR)
            setPadding(dp(16), 0, dp(4), 0)
            minimumHeight = dp(48)
            addView(TextView(this@MainActivity).apply {
                text = "Time app"
                setTextColor(MUTED)
                textSize = 14f
                layoutParams = LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f)
            })
            addView(TextView(this@MainActivity).apply {
                text = "Phone settings  ⚙"
                setTextColor(LIGHT)
                textSize = 15f
                gravity = Gravity.CENTER
                minHeight = dp(48)
                setPadding(dp(12), 0, dp(12), 0)
                isClickable = true
                isFocusable = true
                contentDescription = "Phone settings"
                setOnClickListener { startActivity(Intent(this@MainActivity, SettingsActivity::class.java)) }
            })
        }
        error = TextView(this).apply {
            setTextColor(LIGHT)
            visibility = TextView.GONE
            setPadding(48, 48, 48, 48)
            textSize = 16f
        }
        web = WebView(this).apply {
            // The pages work without JavaScript; it stays on for future
            // interactions and costs nothing on pages that have none.
            settings.javaScriptEnabled = true
            settings.domStorageEnabled = true
            // The site is dark; so is the WebView before the first byte arrives.
            setBackgroundColor(DARK)
            webViewClient = object : WebViewClient() {
                override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
                    // Links to Time stay in the app; anything else opens in the browser.
                    val url = request.url.toString()
                    if (url.startsWith(server.trimEnd('/'))) return false
                    startActivity(Intent(Intent.ACTION_VIEW, request.url))
                    return true
                }

                override fun onReceivedError(view: WebView, request: WebResourceRequest, err: WebResourceError) {
                    if (!request.isForMainFrame) return
                    showError("Can't reach Time at ${server}.\n\n${err.description}\n\n" +
                        "The website is reachable on the home network and over Tailscale. " +
                        "Recording continues either way: the phone keeps what it reads and sends it later.")
                }

                override fun onPageFinished(view: WebView, url: String) {
                    if (error.visibility == TextView.VISIBLE && view.progress == 100 && !url.startsWith("about:")) {
                        error.visibility = TextView.GONE
                        web.visibility = WebView.VISIBLE
                    }
                }
            }
        }
        val retry = Button(this).apply {
            text = "Retry"
            setOnClickListener { load() }
        }
        error.tag = retry

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(DARK)
            addView(bar, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(error, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(retry, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
            addView(web, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        }
        ViewCompat.setOnApplyWindowInsetsListener(root) { v, insets ->
            val b = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout())
            v.setPadding(b.left, b.top, b.right, b.bottom)
            WindowInsetsCompat.CONSUMED
        }
        setContentView(root)
        retry.visibility = Button.GONE

        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                if (web.canGoBack()) web.goBack() else finish()
            }
        })
        load()
    }

    private fun load() {
        error.visibility = TextView.GONE
        (error.tag as Button).visibility = Button.GONE
        web.visibility = WebView.VISIBLE
        web.loadUrl(server.trimEnd('/') + "/")
    }

    private fun showError(text: String) {
        error.text = text
        error.visibility = TextView.VISIBLE
        (error.tag as Button).visibility = Button.VISIBLE
        web.visibility = WebView.GONE
    }
}
