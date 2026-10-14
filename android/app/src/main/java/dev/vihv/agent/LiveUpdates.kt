package dev.vihv.agent

import android.app.NotificationManager
import android.content.Context
import android.os.Build
import android.os.SystemClock
import android.util.Log

/**
 * Android 16（Baklava，API 36）的 **Live Updates**：常驻在状态栏/锁屏芯片里、内容
 * 可以实时变的那条通知（用户眼里的「灵动岛」）。
 *
 * 形状照抄 OpenMinis 的 `DynamicIslandSupport`（连缓存那段踩坑记录一起）：
 *
 * - 设备「支持」要**两个条件同时成立**：跑 16+，**而且**
 *   `NotificationManager.canPostPromotedNotifications()` 为真 —— 后者反映的是
 *   「系统有这个功能 + 用户给这个 App 开了 Live Updates」，是能随时变的（用户去
 *   系统设置里关掉就没了），所以缓存只留 [TTL_MS] 就过期。
 * - 不缓存的话，一个跑着的回合**每秒问两次**这个能力（重画通知 + 建通知各一次），
 *   每次都是一次 binder 往返。OpenMinis 那边实测过：这是他们 profile 里最大的一笔
 *   应用侧开销。5 秒的 TTL 把 90% 的调用省掉，又不会让「刚被授权」等一整个进程。
 * - 16 以下直接假 —— 那是系统版本的事，不可能中途变，也不用问 binder。
 */
object LiveUpdates {

    private const val TAG = "LiveUpdates"

    /** 能力缓存的有效期（见类说明）。 */
    private const val TTL_MS = 5_000L

    @Volatile
    private var cached: Boolean? = null

    @Volatile
    private var cachedAt = 0L

    /** 现在能不能用 Live Updates 这个展示面。 */
    fun active(ctx: Context): Boolean {
        // pre-36 是编译期就定的，不用问系统
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.BAKLAVA) return false
        val c = cached
        if (c != null && SystemClock.elapsedRealtime() - cachedAt < TTL_MS) return c
        return probe(ctx)
    }

    /**
     * 忘掉缓存，下一次重新问系统。
     *
     * 用户可能在系统设置里刚改了 Live Updates 的授权，而 App 刚回到前台正是要重问
     * 的时机（`MainActivity.onResume`）。
     */
    fun invalidate() {
        cached = null
    }

    private fun probe(ctx: Context): Boolean {
        val ok = try {
            ctx.getSystemService(NotificationManager::class.java)
                ?.canPostPromotedNotifications() == true
        } catch (t: Throwable) {
            // 防一手：某些半成品 16 版本这个调用会抛。当「不支持」处理，
            // 于是退回到普通通知 —— 那是降级，不是错误
            Log.w(TAG, "canPostPromotedNotifications() 失败: ${t.message}")
            false
        }
        cached = ok
        cachedAt = SystemClock.elapsedRealtime()
        return ok
    }
}
