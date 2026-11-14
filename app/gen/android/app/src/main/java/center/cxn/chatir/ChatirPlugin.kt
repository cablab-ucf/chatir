package center.cxn.chatir

import android.Manifest.permission.POST_NOTIFICATIONS
import android.app.Activity
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.TrafficStats
import android.os.BatteryManager
import android.os.Process
import android.net.Uri
import android.os.Build
import android.webkit.WebView
import androidx.core.app.ActivityCompat
import androidx.core.app.NotificationChannelCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import androidx.activity.result.ActivityResult
import app.tauri.PermissionState
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class PathArgs {
  var path = ""
}

@InvokeArg
class UrlArgs {
  var url = ""
}

@InvokeArg
class WatchArgs {
  lateinit var channel: Channel
}

@InvokeArg
class AlertArgs {
  var space = ""
  var room = ""
  var title = ""
  var body = ""
  var ring = false
  var hush = false
  var quiet = false
}

@InvokeArg
class ShareArgs {
  var text = ""
}

@InvokeArg
class WriteArgs {
  var path = ""
  var uri = ""
}

@InvokeArg
class OnArgs {
  var on = false
}

@TauriPlugin(permissions = [
  Permission(strings = [android.Manifest.permission.RECORD_AUDIO],
    alias = "microphone"),
])
class ChatirPlugin(private val activity: Activity) : Plugin(activity) {
  override fun load(webView: WebView) {
    if (!notificationsAllowed(activity) && Build.VERSION.SDK_INT >= 33) {
      ActivityCompat.requestPermissions(activity, arrayOf(POST_NOTIFICATIONS), 1)
    }
    // Our lane on the phone's pushir mailbox, rung by storage nodes. No Google.
    live(activity)
  }

  /** Optimized, Doze stops chatir staying connected or waking on push.
   *  Android's prompt first; some phones show none (or it was declined):
   *  then chatir's own settings page, where Battery › Unrestricted is. */
  private var prompted = false
  private fun unrestrict() = runCatching {
    val uri = android.net.Uri.parse("package:" + activity.packageName)
    val act = if (prompted) android.provider.Settings.ACTION_APPLICATION_DETAILS_SETTINGS
      else android.provider.Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS
    prompted = true
    activity.startActivity(Intent(act, uri))
  }

  /** An update chatir downloaded and checked against its signing key,
   *  installed by Android's package installer: from Android 12 on with no
   *  prompt of its own (an app updating itself), once chatir may install
   *  apps, which is asked first. chatir closes as it is replaced. */
  @Command
  fun install(invoke: Invoke) {
    val f = java.io.File(invoke.parseArgs(PathArgs::class.java).path)
    val pm = activity.packageManager
    if (!pm.canRequestPackageInstalls()) {
      runCatching {
        activity.startActivity(Intent(
          android.provider.Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
          android.net.Uri.parse("package:" + activity.packageName)))
      }
      f.delete()
      return invoke.reject("Allow chatir to install apps, then Update again")
    }
    runCatching {
      val p = PackageInstaller.SessionParams(
        PackageInstaller.SessionParams.MODE_FULL_INSTALL)
      p.setAppPackageName(activity.packageName)
      if (Build.VERSION.SDK_INT >= 31) p.setRequireUserAction(
        PackageInstaller.SessionParams.USER_ACTION_NOT_REQUIRED)
      val id = pm.packageInstaller.createSession(p)
      pm.packageInstaller.openSession(id).use { s ->
        s.openWrite("chatir.apk", 0, f.length()).use { out ->
          f.inputStream().use { it.copyTo(out) }
          s.fsync(out)
        }
        // Mutable: the installer adds its answer to it.
        val flags = PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE
        val done = Intent(activity, Installed::class.java)
        s.commit(PendingIntent.getBroadcast(activity, id, done, flags).intentSender)
      }
    }.onSuccess { invoke.resolve() }
      .onFailure { invoke.reject(it.message ?: "could not install") }
    f.delete()
  }

  /** chatir's pushes did not come back to it: its mailbox started anew. */
  @Command
  fun repush(invoke: Invoke) {
    org.pushir.Pushir.stop(activity)
    push(activity)
    invoke.resolve()
  }

  /** "background": chatir exempt, so the phone's pushir connection is
   *  held by an app Android lets run asleep (chatir, if no other is). */
  @Command
  fun alerts(invoke: Invoke) {
    invoke.resolve(JSObject()
      .put("notify", notificationsAllowed(activity))
      .put("background", unrestricted(activity))
      .put("calls", fullScreen(activity)))
  }

  /** This app's traffic since boot, pushir's socket too; the battery. */
  @Command
  fun traffic(invoke: Invoke) {
    val u = Process.myUid()
    val b = activity.getSystemService(BatteryManager::class.java)
    val pct = b?.getIntProperty(BatteryManager.BATTERY_PROPERTY_CAPACITY)
    invoke.resolve(JSObject()
      .put("tx_packets", TrafficStats.getUidTxPackets(u))
      .put("rx_packets", TrafficStats.getUidRxPackets(u))
      .put("tx_bytes", TrafficStats.getUidTxBytes(u))
      .put("rx_bytes", TrafficStats.getUidRxBytes(u))
      .put("battery", pct))
  }

  /** Android's own count of each app's battery. */
  @Command
  fun batteryUse(invoke: Invoke) {
    runCatching {
      activity.startActivity(Intent(Intent.ACTION_POWER_USAGE_SUMMARY))
    }
    invoke.resolve()
  }

  @Command
  fun allowAlerts(invoke: Invoke) {
    if (!notificationsAllowed(activity)) runCatching {
      val i = Intent(android.provider.Settings.ACTION_APP_NOTIFICATION_SETTINGS)
        .putExtra(android.provider.Settings.EXTRA_APP_PACKAGE,
          activity.packageName)
      activity.startActivity(i)
    } else if (!unrestricted(activity)) unrestrict()
    else if (!fullScreen(activity)) runCatching {
      val uri = android.net.Uri.parse("package:" + activity.packageName)
      activity.startActivity(Intent(
        android.provider.Settings.ACTION_MANAGE_APP_USE_FULL_SCREEN_INTENT, uri
      ))
    }
    invoke.resolve()
  }

  @Command
  fun testAlert(invoke: Invoke) {
    postMessage(activity, "test", "chatir", "Notifications reach this phone.")
    invoke.resolve()
  }

  /** Native code sees neither network changes nor whether chatir is in sight. */
  @Command
  fun watchNetwork(invoke: Invoke) {
    val channel = invoke.parseArgs(WatchArgs::class.java).channel
    val cm = activity.getSystemService(ConnectivityManager::class.java)
    val ping = object : ConnectivityManager.NetworkCallback() {
      override fun onAvailable(network: Network) = channel.send(JSObject())
      override fun onLost(network: Network) = channel.send(JSObject())
      override fun onLinkPropertiesChanged(n: Network, lp: LinkProperties) =
        channel.send(JSObject())
    }
    cm.registerDefaultNetworkCallback(ping)
    val seen = object : DefaultLifecycleObserver {
      override fun onStart(owner: LifecycleOwner) =
        channel.send(JSObject().put("fg", true))
      override fun onStop(owner: LifecycleOwner) =
        channel.send(JSObject().put("fg", false))
    }
    activity.runOnUiThread {
      ProcessLifecycleOwner.get().lifecycle.addObserver(seen)
    }
    invoke.resolve()
  }

  @Command
  fun alert(invoke: Invoke) {
    val a = invoke.parseArgs(AlertArgs::class.java)
    alert(activity, org.json.JSONObject().put("space", a.space)
      .put("room", a.room).put("title", a.title).put("body", a.body)
      .put("ring", a.ring).put("hush", a.hush).put("quiet", a.quiet))
    invoke.resolve()
  }

  @Command
  fun hush(invoke: Invoke) {
    hush(activity)
    invoke.resolve()
  }

  /** A call: the screen on, over the lock screen, and the microphone
   *  allowed before chatir's own voice opens it. */
  @Command
  fun inCall(invoke: Invoke) {
    val on = invoke.parseArgs(OnArgs::class.java).on
    activity.runOnUiThread { (activity as? MainActivity)?.overLock(on) }
    // In a call as a phone is: volume keys for the call, its routes.
    (activity.getSystemService(Context.AUDIO_SERVICE) as android.media.AudioManager)
      .mode = if (on) android.media.AudioManager.MODE_IN_COMMUNICATION
        else android.media.AudioManager.MODE_NORMAL
    if (on && getPermissionState("microphone") != PermissionState.GRANTED) {
      requestPermissionForAlias("microphone", invoke, "allowed")
    } else invoke.resolve()
  }

  @PermissionCallback
  private fun allowed(invoke: Invoke) = invoke.resolve()

  /** "Private notifications", as the page reads or switches it. */
  @Command
  fun veil(invoke: Invoke) {
    val on = invoke.parseArgs(OnArgs::class.java).on
    activity.runOnUiThread { (activity as? MainActivity)?.veil(on) }
    invoke.resolve()
  }

  @Command
  fun share(invoke: Invoke) {
    val text = invoke.parseArgs(ShareArgs::class.java).text
    val send = Intent(Intent.ACTION_SEND).setType("text/plain")
      .putExtra(Intent.EXTRA_TEXT, text)
    activity.startActivity(Intent.createChooser(send, null))
    invoke.resolve()
  }

  @Command
  fun browse(invoke: Invoke) {
    val url = invoke.parseArgs(UrlArgs::class.java).url
    val i = Intent(Intent.ACTION_VIEW, android.net.Uri.parse(url))
    try {
      activity.startActivity(i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
      invoke.resolve()
    } catch (e: android.content.ActivityNotFoundException) {
      invoke.reject("no app opens it")
    }
  }

  /** Android's own "save as": where to, or nothing if put away. */
  @Command
  fun saveAs(invoke: Invoke) {
    val name = invoke.parseArgs(ShareArgs::class.java).text
    val type = java.net.URLConnection.guessContentTypeFromName(name)
    val i = Intent(Intent.ACTION_CREATE_DOCUMENT)
      .addCategory(Intent.CATEGORY_OPENABLE)
      .setType(type ?: "application/octet-stream")
      .putExtra(Intent.EXTRA_TITLE, name)
    startActivityForResult(invoke, i, "picked")
  }

  @ActivityCallback
  private fun picked(invoke: Invoke, r: ActivityResult) {
    val uri = r.data?.data?.takeIf { r.resultCode == Activity.RESULT_OK }
    invoke.resolve(if (uri == null) JSObject() else JSObject().put("uri", "$uri"))
  }

  /** A saved file to where `saveAs` said: off the main thread, as large
   *  ones take a while. */
  @Command
  fun writeTo(invoke: Invoke) {
    val a = invoke.parseArgs(WriteArgs::class.java)
    Thread {
      val file = java.io.File(a.path)
      runCatching {
        val out = activity.contentResolver.openOutputStream(Uri.parse(a.uri), "wt")
        (out ?: error("cannot write there")).use { o ->
          file.inputStream().use { it.copyTo(o) }
        }
      }.onSuccess { invoke.resolve() }
        .onFailure { invoke.reject("saving: ${it.message}") }
      file.delete()
    }.start()
  }
}

fun unrestricted(ctx: Context) =
  ctx.getSystemService(android.os.PowerManager::class.java)
    .isIgnoringBatteryOptimizations(ctx.packageName)

/** Android 14 on: a call fills a locked screen only if allowed. */
fun fullScreen(ctx: Context) = Build.VERSION.SDK_INT < 34 ||
  ctx.getSystemService(android.app.NotificationManager::class.java)
    .canUseFullScreenIntent()

fun notificationsAllowed(ctx: Context) = Build.VERSION.SDK_INT < 33 ||
  ContextCompat.checkSelfPermission(ctx, POST_NOTIFICATIONS) ==
  PackageManager.PERMISSION_GRANTED

fun openAppIntent(ctx: Context): PendingIntent = PendingIntent.getActivity(
  ctx, 0, Intent(ctx, MainActivity::class.java),
  PendingIntent.FLAG_IMMUTABLE,
)

fun notificationChannel(ctx: Context, id: String, name: String, level: Int) =
  NotificationManagerCompat.from(ctx).createNotificationChannel(
    NotificationChannelCompat.Builder(id, level).setName(name).build()
  )

/** The package installer's answer: its prompt shown when it asks one (an
 *  update before Android 12), a failure said. */
class Installed : BroadcastReceiver() {
  override fun onReceive(c: Context, i: Intent) {
    when (i.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)) {
      PackageInstaller.STATUS_SUCCESS -> Unit
      PackageInstaller.STATUS_PENDING_USER_ACTION -> {
        @Suppress("DEPRECATION")
        val ask = i.getParcelableExtra<Intent>(Intent.EXTRA_INTENT) ?: return
        runCatching { c.startActivity(ask.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)) }
      }
      else -> android.widget.Toast.makeText(c, "Update failed: " +
        (i.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE) ?: "?"),
        android.widget.Toast.LENGTH_LONG).show()
    }
  }
}
