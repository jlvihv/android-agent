package dev.vihv.agent

import android.content.ComponentName
import android.content.Context
import android.provider.Settings
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * 通知读取（`agent notifications`）：系统的通知监听服务。
 *
 * 授权方式和别的权限不一样：**没有运行时弹框**，得用户自己去系统设置的「通知使用权」里
 * 打开 —— 设置页「手机权限」那一栏有入口（[openSettings]）。
 *
 * 两份数据：
 * - **现在通知栏里的**：直接问系统（`activeNotifications`）
 * - **最近收到过的**（包括已经划掉的）：服务连着时自己记一个环形缓冲，最多 [MAX_RECENT] 条。
 *   只在内存里 —— 进程重启就清空；通知内容是别人的私信、验证码，不往盘上写
 */
class AgentNotificationListener : NotificationListenerService() {

    override fun onListenerConnected() {
        instance = this
        Log.i(TAG, "通知读取已连上")
    }

    override fun onListenerDisconnected() {
        if (instance === this) instance = null
    }

    override fun onNotificationPosted(sbn: StatusBarNotification) {
        // 正在进行的（下载进度、音乐播放、我们自己的回合通知）不记：它们一直在刷新，
        // 会把真正的消息挤出缓冲区
        if (sbn.isOngoing) return
        val item = describe(this, sbn) ?: return
        synchronized(buffer) {
            buffer.addFirst(item)
            while (buffer.size > MAX_RECENT) buffer.removeLast()
        }
    }

    companion object {
        private const val TAG = "AgentNotifications"
        private const val MAX_RECENT = 100

        @Volatile
        private var instance: AgentNotificationListener? = null

        private val buffer = ArrayDeque<JSONObject>()

        /** 用户在系统设置里给了「通知使用权」没有。 */
        fun enabled(ctx: Context): Boolean {
            val me = ComponentName(ctx, AgentNotificationListener::class.java).flattenToString()
            val list = Settings.Secure.getString(ctx.contentResolver, "enabled_notification_listeners").orEmpty()
            return list.split(':').any { it == me }
        }

        /** 系统设置里「通知使用权」那一页（能直达本 App 的就直达）。 */
        fun settingsIntent(ctx: Context): android.content.Intent =
            if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.R) {
                android.content.Intent(Settings.ACTION_NOTIFICATION_LISTENER_DETAIL_SETTINGS).putExtra(
                    Settings.EXTRA_NOTIFICATION_LISTENER_COMPONENT_NAME,
                    ComponentName(ctx, AgentNotificationListener::class.java).flattenToString(),
                )
            } else {
                android.content.Intent("android.settings.ACTION_NOTIFICATION_LISTENER_SETTINGS")
            }

        /** 现在通知栏里的（最新在前）。服务没连上就是 null。 */
        fun active(ctx: Context, app: String?, limit: Int): JSONArray? {
            val svc = instance ?: return null
            val items = runCatching { svc.activeNotifications }.getOrNull() ?: return null
            val out = JSONArray()
            items.sortedByDescending { it.postTime }
                .filter { app == null || it.packageName == app }
                .mapNotNull { describe(ctx, it) }
                .take(limit)
                .forEach { out.put(it) }
            return out
        }

        /** 最近收到过的（含已划掉的，最新在前）。 */
        fun recent(app: String?, limit: Int): JSONArray {
            val out = JSONArray()
            synchronized(buffer) {
                buffer.asSequence()
                    .filter { app == null || it.optString("package") == app }
                    .take(limit)
                    .forEach { out.put(it) }
            }
            return out
        }

        fun connected(): Boolean = instance != null

        private val timeFmt = SimpleDateFormat("yyyy-MM-dd'T'HH:mm:ss", Locale.US)

        private fun describe(ctx: Context, sbn: StatusBarNotification): JSONObject? {
            val extras = sbn.notification?.extras ?: return null
            val title = extras.getCharSequence(android.app.Notification.EXTRA_TITLE)?.toString().orEmpty()
            // 展开后的长文本优先（聊天类通知的 text 常常只是「2 条新消息」）
            val text = (
                extras.getCharSequence(android.app.Notification.EXTRA_BIG_TEXT)
                    ?: extras.getCharSequence(android.app.Notification.EXTRA_TEXT)
                )?.toString().orEmpty()
            if (title.isEmpty() && text.isEmpty()) return null
            val label = runCatching {
                val pm = ctx.packageManager
                pm.getApplicationLabel(pm.getApplicationInfo(sbn.packageName, 0)).toString()
            }.getOrDefault(sbn.packageName)
            return JSONObject()
                .put("time", timeFmt.format(Date(sbn.postTime)))
                .put("app", label)
                .put("package", sbn.packageName)
                .put("title", title)
                .put("text", text)
        }
    }
}
