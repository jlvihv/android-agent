package dev.vihv.agent

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.content.Intent
import android.graphics.Path
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.accessibility.AccessibilityEvent
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * 设备操控。这是"操控其他 App 的唯一途径"（Android 框架限制）。
 *
 * 现在它只做**不需要读界面**的那几件事：坐标点击、滑动、系统按键、启动应用。
 *
  * 读界面那套（把无障碍树拍平成 JSON 推给 Rust）不在：`device_snapshot` /
 * `device_tap` 那两个工具已经删了，没人要它。以后要恢复的话做法不变：树在
 * native 侧拍平成 JSON 推给 Rust，Rust 回传指令，**native 侧一行反调 Java 的
 * 代码都没有**（决策 D2）。
 */
class AgentAccessibilityService : AccessibilityService() {

    companion object {
        private const val TAG = "AgentA11y"

        @Volatile
        var instance: AgentAccessibilityService? = null
            private set
    }

    private val main = Handler(Looper.getMainLooper())

    override fun onServiceConnected() {
        super.onServiceConnected()
        instance = this
        Log.i(TAG, "无障碍服务已连接")
        AgentApp.onDeviceReady(this)
    }

    override fun onUnbind(intent: Intent?): Boolean {
        instance = null
        AgentApp.onDeviceGone()
        return super.onUnbind(intent)
    }

    override fun onInterrupt() {
        Log.w(TAG, "无障碍服务被中断")
    }

    /**
     * 框架要求实现；不读界面，这里什么都不用做。
     */
    override fun onAccessibilityEvent(event: AccessibilityEvent?) {
        // 故意留空
    }

    // -----------------------------------------------------------------------
    // 指令
    // -----------------------------------------------------------------------

    fun tapXY(x: Int, y: Int): String = onMain {
        tapAt(x.toFloat(), y.toFloat())
        "tapped ($x,$y)"
    }

    fun swipe(x1: Int, y1: Int, x2: Int, y2: Int, ms: Int): String = onMain {
        val path = Path().apply {
            moveTo(x1.toFloat(), y1.toFloat())
            lineTo(x2.toFloat(), y2.toFloat())
        }
        val gesture = GestureDescription.Builder()
            .addStroke(GestureDescription.StrokeDescription(path, 0L, ms.coerceIn(50, 3000).toLong()))
            .build()
        if (!dispatchGesture(gesture, null, null)) error("滑动手势被拒绝")
        "swiped ($x1,$y1)->($x2,$y2)"
    }

    fun pressKey(name: String): String = onMain {
        val action = when (name) {
            "back" -> GLOBAL_ACTION_BACK
            "home" -> GLOBAL_ACTION_HOME
            "recents" -> GLOBAL_ACTION_RECENTS
            "notifications" -> GLOBAL_ACTION_NOTIFICATIONS
            else -> error("不支持的按键: $name")
        }
        if (!performGlobalAction(action)) error("performGlobalAction($name) 被拒绝")
        "pressed $name"
    }

    fun launch(pkg: String): String = onMain {
        val intent = packageManager.getLaunchIntentForPackage(pkg)
            ?: error("找不到应用 $pkg（没装？）")
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        startActivity(intent)
        "launched $pkg"
    }

    // -----------------------------------------------------------------------
    // 内部
    // -----------------------------------------------------------------------

    private fun tapAt(x: Float, y: Float) {
        val path = Path().apply { moveTo(x, y) }
        val gesture = GestureDescription.Builder()
            .addStroke(GestureDescription.StrokeDescription(path, 0L, 60L))
            .build()
        if (!dispatchGesture(gesture, null, null)) error("点击手势被拒绝")
    }

    /**
     * 无障碍 API 必须在主线程调。
     *
     * `execute` 是从 Rust 的 `spawn_blocking` 线程进来的，所以这里
     * post 到主线程并**阻塞等结果** —— 调用方（Rust）本来就在阻塞线程上，
     * 而且单次调用只有几毫秒。
     */
    private fun <T> onMain(block: () -> T): T = onThread(main, block)

    /** 在 `h` 的线程上跑 `block` 并阻塞等结果 */
    private fun <T> onThread(h: Handler, block: () -> T): T {
        if (Looper.myLooper() == h.looper) return block()

        val latch = CountDownLatch(1)
        var result: T? = null
        var error: Throwable? = null

        h.post {
            try {
                result = block()
            } catch (t: Throwable) {
                error = t
            } finally {
                latch.countDown()
            }
        }

        if (!latch.await(8, TimeUnit.SECONDS)) {
            error("${h.looper.thread.name} 线程执行超时（8s）")
        }
        error?.let { throw it }
        @Suppress("UNCHECKED_CAST")
        return result as T
    }
}
