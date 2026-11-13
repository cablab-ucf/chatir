package center.cxn.chatir

import android.app.KeyguardManager
import android.content.Intent
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.view.View
import android.view.WindowManager
import android.webkit.WebView
import androidx.activity.OnBackPressedCallback
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  private var web: WebView? = null
  // Over a locked phone now: the page asks as it loads (`chatirLock`), for
  // a call that rings before there is a page to tell.
  @Volatile private var locked = false
  // "Private notifications": nothing of chatir in screenshots or recents,
  // nor over the lock screen, where a ring would name its caller (there,
  // its notification says "Incoming call", and Answer asks to unlock).
  private var veiled = false

  override fun onCreate(savedInstanceState: Bundle?) {
    // The UI is dark: light system-bar icons whatever the system theme.
    val bars = SystemBarStyle.dark(Color.TRANSPARENT)
    enableEdgeToEdge(bars, bars)
    Wake.release() // a push may have started chatir first
    veil(Wake.veiled(dataDir.absolutePath))
    answerable(intent)
    overLock(ringing(intent))
    super.onCreate(savedInstanceState)
    // Edge to edge: inset the page by the bars, cutouts and keyboard.
    val root = findViewById<View>(android.R.id.content)
    ViewCompat.setOnApplyWindowInsetsListener(root) { view, insets ->
      val i = insets.getInsets(
        WindowInsetsCompat.Type.systemBars() or
          WindowInsetsCompat.Type.displayCutout() or
          WindowInsetsCompat.Type.ime()
      )
      view.setPadding(i.left, i.top, i.right, i.bottom)
      WindowInsetsCompat.CONSUMED
    }
  }

  override fun onNewIntent(intent: Intent) {
    answerable(intent)
    if (ringing(intent)) overLock(true)
    super.onNewIntent(intent)
  }

  // Out of sight, off the lock screen: back over it only as a call rings
  // or the page, in a call, says so again.
  override fun onStop() {
    overLock(false)
    super.onStop()
  }

  private fun ringing(i: Intent?) = i?.data?.let {
    it.scheme == "chatir" && (it.host == "ring" || it.host == "call")
  } == true

  fun veil(on: Boolean) {
    veiled = on
    val f = WindowManager.LayoutParams.FLAG_SECURE
    if (on) window.addFlags(f) else window.clearFlags(f)
  }

  /** A ringing call, or one under way: shown over the lock screen (not
   *  veiled), the screen lit and kept on. Only then, never for the rest
   *  of chatir. */
  @Suppress("DEPRECATION")
  fun overLock(on: Boolean) {
    val show = on && !veiled
    if (Build.VERSION.SDK_INT >= 27) {
      setShowWhenLocked(show)
      setTurnScreenOn(on)
    } else {
      val s = WindowManager.LayoutParams.FLAG_SHOW_WHEN_LOCKED
      val t = WindowManager.LayoutParams.FLAG_TURN_SCREEN_ON
      if (show) window.addFlags(s) else window.clearFlags(s)
      if (on) window.addFlags(t) else window.clearFlags(t)
    }
    val keep = WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON
    if (on) window.addFlags(keep) else window.clearFlags(keep)
    // Over a locked phone, the call alone: no way into the rest of chatir.
    locked = show && getSystemService(KeyguardManager::class.java)
      .isKeyguardLocked
    web?.evaluateJavascript(
      "document.documentElement.classList.toggle('lockscreen', $locked)", null)
  }

  // Back closes what the page has open; else chatir goes to the background,
  // as finishing would stop the node.
  override fun onWebViewCreate(webView: WebView) {
    web = webView
    webView.addJavascriptInterface(object {
      @android.webkit.JavascriptInterface fun locked() = locked
    }, "chatirLock")
    val back = object : OnBackPressedCallback(true) {
      override fun handleOnBackPressed() {
        webView.evaluateJavascript("window.chatirBack?.() === true") { handled ->
          if (handled != "true") moveTaskToBack(true)
        }
      }
    }
    onBackPressedDispatcher.addCallback(this, back)
  }
}
