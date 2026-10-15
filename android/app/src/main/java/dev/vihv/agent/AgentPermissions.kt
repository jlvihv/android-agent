package dev.vihv.agent

import android.app.Activity
import android.content.Context
import android.content.pm.PackageManager
import android.util.Log
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * 运行时权限：把「弹窗 + 等用户点」包成一次**阻塞**调用。
 *
 * 为什么必须同步：设备命令（`agent contacts list` 之类）跑在 Rust 的
 * `spawn_blocking` 线程上，它要的就是一个答案；而弹窗只能在主线程发、
 * 结果只能从 `onRequestPermissionsResult` 回。这里用一把闩把两头接上。
 *
 * 三个刻意的选择：
 *
 * 1. **只在人真的在的时候弹**（`MainActivity.current` 不为空）。后台偷偷弹权限框
 *    在 Android 10+ 根本不显示，用户回来只看到一条「超时」。
 * 2. **问过就不问**：Android 只对「第二次及以后的拒绝」才显示成"不再询问"，
 *    但对我们的用处一样 —— 系统不会再把窗口弹出来。所以自己记一笔，
 *    下次直接返回「没拿到」，让模型去解释「去设置里开」，而不是干等一次超时。
 * 3. 超时 25s：人不在就别磨着（下一次会直接告诉模型「问过了没给」）。
 */
object AgentPermissions {
    private const val TAG = "AgentPermissions"

    /** 请求码：和 MainActivity 自己用的那几个（2..5）错开，别互相认错。 */
    const val REQUEST_CODE = 0x4A11

    private const val PREFS = "agent_permissions"

    /**
     * 等多久。
     *
     * **刻意短**：人要真在看着屏幕，几秒就点了；等到 25 秒还没点，基本就是
     * 人不在（手机在口袋里、在开会）。一个 agent 回合里白等 90 秒是很糟的体验，
     * 而"再来一次"的代价低得多 —— 加上问过就记一笔（[key]），同一个人不会
     * 被同一个弹框拦两次。
     */
    private const val TIMEOUT_MS = 25_000L

    @Volatile
    private var latch: CountDownLatch? = null

    @Volatile
    private var reply: IntArray? = null

    /**
     * 拿到这些权限（该弹就弹），返回**没拿到的**那几个（空 = 全有了）。
     *
     * 调用方在工作线程上，所以这里会阻塞 —— 这是这个类的全部意义。
     */
    fun ensure(context: Context, vararg perms: String): List<String> {
        val app = context.applicationContext
        val missing = perms.filter { !has(app, it) }
        if (missing.isEmpty()) return emptyList()

        val activity = MainActivity.current
        if (activity == null) {
            Log.w(TAG, "没人在前台，弹不了权限框：$missing")
            return missing
        }

        val prefs = app.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val askable = missing.filter { !prefs.getBoolean(key(it), false) }
        if (askable.isEmpty()) {
            Log.w(TAG, "这些问过且没给，得去设置里手动开：$missing")
            return missing
        }

        val l = CountDownLatch(1)
        latch = l
        reply = null
        activity.runOnUiThread {
            runCatching { activity.requestPermissions(askable.toTypedArray(), REQUEST_CODE) }
                .onFailure {
                    Log.w(TAG, "权限弹窗没发出去", it)
                    l.countDown()
                }
        }

        val answered = l.await(TIMEOUT_MS, TimeUnit.MILLISECONDS)
        latch = null
        reply = null

        // 问过一次就记下来 —— 无论结果如何。下次不再假装在等。
        prefs.edit().apply { askable.forEach { putBoolean(key(it), true) } }.apply()

        if (!answered) Log.w(TAG, "等权限超时（人没点）：$askable")
        // 以系统现在报的为准（用户可能勾了"不再询问"又去设置里开了）
        return missing.filter { !has(app, it) }
    }

    /** 给人看的一句话（拼进设备命令的错误消息里）。 */
    fun humanName(perm: String): String = when (perm) {
        android.Manifest.permission.ACCESS_FINE_LOCATION,
        android.Manifest.permission.ACCESS_COARSE_LOCATION -> "定位"
        android.Manifest.permission.READ_CONTACTS -> "联系人（读）"
        android.Manifest.permission.WRITE_CONTACTS -> "联系人（改）"
        android.Manifest.permission.READ_CALENDAR -> "日历（读）"
        android.Manifest.permission.WRITE_CALENDAR -> "日历（改）"
        android.Manifest.permission.READ_SMS -> "短信（读）"
        android.Manifest.permission.SEND_SMS -> "短信（发）"
        android.Manifest.permission.RECORD_AUDIO -> "录音"
        android.Manifest.permission.CAMERA -> "相机"
        android.Manifest.permission.POST_NOTIFICATIONS -> "通知"
        else -> perm.substringAfterLast('.')
    }

    /**
     * `MainActivity.onRequestPermissionsResult` 转发过来。
     *
     * 请求码不是我们的就直接忽略 —— Activity 自己还发着几个别的请求。
     */
    fun onResult(requestCode: Int, grantResults: IntArray) {
        if (requestCode != REQUEST_CODE) return
        reply = grantResults
        latch?.countDown()
    }

    private fun has(context: Context, perm: String) =
        context.checkSelfPermission(perm) == PackageManager.PERMISSION_GRANTED

    private fun key(perm: String) = "asked_$perm"

    /** 仅测试/调试：忘掉「问过」的记录，下次还会弹。 */
    fun forget(context: Context) {
        context.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().clear()
            .apply()
    }

    /** 当前有没有活着的 Activity（`agent device` 会报给模型，好解释为什么弹不出来）。 */
    fun activityAvailable(): Boolean = MainActivity.current is Activity
}
