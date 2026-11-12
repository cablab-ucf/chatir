package center.cxn.chatir

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import org.pushir.Pushir
import org.json.JSONArray

/** Rust without Tauri: a push may come when no activity is running. */
object Wake {
  init { System.loadLibrary("chatir_app") }

  @JvmStatic external fun run(ctx: Context, dir: String, full: Boolean): String

  /** A call declined from its notification: its caller hears so. */
  @JvmStatic external fun decline(space: String, room: String)

  /** Rung through a storage node, else on our lane alone. */
  @JvmStatic external fun live(ctx: Context, dir: String): String

  /** Hand Android's context back before the activity's runtime takes it. */
  @JvmStatic external fun release()

  /** chatir's slot on the phone's mailbox (random, kept). */
  @JvmStatic external fun slot(dir: String): ByteArray

  /** The phone's mailbox id, from pushir's library ("" until known). */
  @JvmStatic external fun share(id: String)

  /** What pushir brought: to chatir's node if it runs, else
   *  `{show: notices, catch: whether to catch up}`. */
  @JvmStatic external fun ring(ctx: Context, dir: String, data: ByteArray): String
}

/** The phone's one mailbox, shared with every pushir app (pushir's library:
 *  its owner holds the socket and its heartbeats). A ring wakes chatir here,
 *  running or not. */
class ChatirApp : android.app.Application() {
  override fun onCreate() {
    super.onCreate()
    val dir = dataDir.absolutePath
    Wake.share("")
    val seen = getSharedPreferences("chatir-push", 0)
    Pushir.start(this, "chatir_app", Wake.slot(dir), onId = { id ->
      Wake.share(id)
      // A new mailbox: members told where.
      if (seen.getString("id", null) != id) {
        seen.edit().putString("id", id).apply()
        live(this)
      }
    }) { d ->
      Thread {
        if (show(this, Wake.ring(applicationContext, dir, d))) wake(this)
      }.start()
    }
  }
}

/** A push lends a few foreground seconds to start a service; refused
 *  (Android 12+, battery optimized), chatir catches up in those seconds. */
fun wake(ctx: Context) {
  val i = Intent(ctx, ChatirWake::class.java)
  runCatching { ctx.startForegroundService(i) }.onFailure {
    Thread { catchUp(ctx) }.start()
  }
}

/** A ring's notices shown; true if chatir must catch up for it (a message
 *  too big to carry, a DM to take): most need no node at all. */
fun show(ctx: Context, json: String): Boolean = runCatching {
  val got = org.json.JSONObject(json)
  val show = got.getJSONArray("show")
  for (k in 0 until show.length()) alert(ctx, show.getJSONObject(k))
  got.getBoolean("catch")
}.getOrDefault(true)

/** CPU held awake: asleep, Android would freeze chatir mid-sync. */
fun catchUp(ctx: Context) {
  val pm = ctx.getSystemService(Context.POWER_SERVICE) as android.os.PowerManager
  val lock = pm.newWakeLock(android.os.PowerManager.PARTIAL_WAKE_LOCK, "chatir:catch-up")
  lock.acquire(60_000)
  try {
    for (full in listOf(false, true)) runCatching {
      val dir = ctx.dataDir.absolutePath
      val got = JSONArray(Wake.run(ctx.applicationContext, dir, full))
      for (k in 0 until got.length()) alert(ctx, got.getJSONObject(k))
    }
  } finally {
    if (lock.isHeld) lock.release()
  }
}

class ChatirWake : Service() {
  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, id: Int): Int {
    val quiet = NotificationManagerCompat.IMPORTANCE_MIN
    notificationChannel(this, SYNCING, "Checking for messages", quiet)
    val n = NotificationCompat.Builder(this, SYNCING)
      .setSmallIcon(R.drawable.ic_stat)
      .setContentTitle("Checking for messages")
      .build()
    if (Build.VERSION.SDK_INT >= 34) {
      startForeground(2, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SHORT_SERVICE)
    } else {
      startForeground(2, n)
    }
    Thread {
      catchUp(this)
      stopSelf(id)
    }.start()
    return START_NOT_STICKY
  }

  // Android ends a short service after 3 minutes; ours ends sooner.
  override fun onTimeout(startId: Int, fgsType: Int) = stopSelf()
}

const val SYNCING = "syncing"

/** chatir's lane told, and its route to a storage node that takes it. */
fun live(ctx: Context) {
  val dir = ctx.dataDir.absolutePath
  Thread { runCatching { Wake.live(ctx.applicationContext, dir) } }.start()
}
