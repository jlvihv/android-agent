package dev.vihv.agent

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.content.Intent
import android.graphics.Path
import android.graphics.Rect
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.os.Looper
import android.util.Log
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * 设备操控。这是"操控其他 App 的唯一途径"（Android 框架限制）。
 *
 * 设计铁律（docs/01-tech-selection.md §4）：**native 侧不反调 Java**。
 * 所以这里做的是：把无障碍树拍平成 JSON 推给 Rust，Rust 回传指令，
 * 我们在这里执行。Rust 那边一行反调 Java 的代码都没有。
 */
class AgentAccessibilityService : AccessibilityService() {

    companion object {
        private const val TAG = "AgentA11y"

        /** 事件合并窗口。无障碍事件每秒可达几十次，不快照会被打爆。 */
        private const val DEBOUNCE_MS = 250L

        @Volatile
        var instance: AgentAccessibilityService? = null
            private set
    }

    private val main = Handler(Looper.getMainLooper())

    /**
     * 快照专用线程。
     *
     * 原来快照在主线程做 —— 无障碍服务和 WebView 在**同一个进程**，主线程
     * 就是我们自己 UI 的线程。遍历 500 个节点、每个 getChild 都是一次跨进程
     * 调用，再拼 JSON，一次几十到上百毫秒，用户在 App 里划动时就是掉帧。
     * 节点查询本身是线程安全的，不需要主线程。
     */
    private val worker = Handler(HandlerThread("a11y-snapshot").apply { start() }.looper)
    private var pendingPush: Runnable? = null

    @Volatile
    private var lastActivity: String = ""

    /** 上次推给 Rust 的界面内容（不含时间戳）。没变就不推。 */
    private var lastKey: String? = null

    /** 最近一次快照的节点定位信息，指令执行时按索引查 */
    @Volatile
    private var refs: Map<Int, UiTree.NodeRef> = emptyMap()

    // -----------------------------------------------------------------------
    // 生命周期
    // -----------------------------------------------------------------------

    override fun onServiceConnected() {
        super.onServiceConnected()
        instance = this
        Log.i(TAG, "无障碍服务已连接")
        AgentApp.onDeviceReady(this)
    }

    override fun onUnbind(intent: Intent?): Boolean {
        instance = null
        worker.looper.quitSafely()
        AgentApp.onDeviceGone()
        Log.i(TAG, "无障碍服务已断开")
        return super.onUnbind(intent)
    }

    override fun onInterrupt() {
        Log.w(TAG, "无障碍服务被中断")
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {
        val e = event ?: return
        // 自己的界面不用盯。用户在 App 里划动、流式输出时 WebView 每秒几十个
        // 事件，每个都引来一次快照 + 一次广播给前端，前端又因此重绘 —— 白费，
        // 还拖慢正在看的那个界面。agent 要看自己时会显式调 snapshot。
        if (e.packageName?.toString() == packageName) return
        if (e.eventType == AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED) {
            e.className?.toString()?.let { lastActivity = it }
        }
        schedulePush()
    }

    /** 合并密集事件：取消上一个待执行的，250ms 后再快照 */
    private fun schedulePush() {
        pendingPush?.let { worker.removeCallbacks(it) }
        val r = Runnable {
            pendingPush = null
            pushSnapshot(force = false)
        }
        pendingPush = r
        worker.postDelayed(r, DEBOUNCE_MS)
    }

    // -----------------------------------------------------------------------
    // 快照
    // -----------------------------------------------------------------------

    /**
     * 读一次界面并推给 Rust。返回节点数；失败返回 -1。
     *
     * `force = false`（事件触发的）时界面没变就不推 —— 每次推送都会广播
     * 给前端，几百个节点的 JSON。agent 显式要快照时总是推。
     */
    fun pushSnapshot(force: Boolean = true): Int = onThread(worker) {
        val snap = UiTree.snapshot(this, lastActivity)
        if (snap == null) {
            Log.w(TAG, "拿不到根节点，跳过本次快照")
            return@onThread -1
        }
        refs = snap.refs
        if (!force && snap.key == lastKey) return@onThread snap.refs.size
        try {
            AgentApp.core.pushSnapshot(snap.json)
            lastKey = snap.key
            snap.refs.size
        } catch (t: Throwable) {
            Log.e(TAG, "推送快照失败", t)
            -1
        }
    }

    // -----------------------------------------------------------------------
    // 指令
    // -----------------------------------------------------------------------

    fun tapNode(i: Int): String = onMain {
        val ref = refs[i] ?: error("快照里没有节点 $i（界面可能已经变了，先刷新）")

        // 优先按 viewId 找节点做 performAction —— 比坐标稳，
        // 不受滚动位置和状态栏高度影响
        if (ref.viewId != null) {
            val found = rootInActiveWindow
                ?.findAccessibilityNodeInfosByViewId(ref.viewId)
                ?.firstOrNull { it.isClickable || it.isVisibleToUser }
            if (found != null) {
                val ok = found.performAction(AccessibilityNodeInfo.ACTION_CLICK)
                @Suppress("DEPRECATION")
                runCatching { found.recycle() }
                if (ok) return@onMain "clicked #$i via viewId"
            }
        }

        // 退回坐标点击。模型看到的 bounds 和这里用的是同一份，
        // 所以两者一致
        val cx = ref.bounds.exactCenterX()
        val cy = ref.bounds.exactCenterY()
        if (cx <= 0f && cy <= 0f) error("节点 $i 的 bounds 是空的")
        tapAt(cx, cy)
        "tapped #$i at (${cx.toInt()},${cy.toInt()})"
    }

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

    fun setText(i: Int, text: String): String = onMain {
        val ref = refs[i] ?: error("快照里没有节点 $i")
        val node = findNode(ref) ?: error("找不到节点 $i 对应的控件")
        try {
            if (!node.isEditable) error("节点 $i 不是输入框（${node.className}）")
            // 先聚焦，再设置文本 —— 有些输入法要焦点才认
            node.performAction(AccessibilityNodeInfo.ACTION_FOCUS)
            val args = Bundle().apply {
                putCharSequence(
                    AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE,
                    text,
                )
            }
            if (!node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)) {
                error("ACTION_SET_TEXT 被拒绝")
            }
            "typed ${text.length} chars into #$i"
        } finally {
            @Suppress("DEPRECATION")
            runCatching { node.recycle() }
        }
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

    private fun findNode(ref: UiTree.NodeRef): AccessibilityNodeInfo? {
        val root = rootInActiveWindow ?: return null
        if (ref.viewId != null) {
            root.findAccessibilityNodeInfosByViewId(ref.viewId)?.firstOrNull()?.let { return it }
        }
        return findByBounds(root, ref.bounds, 0)
    }

    /** 按 bounds 递归找（有些控件没有 viewId） */
    private fun findByBounds(
        node: AccessibilityNodeInfo,
        target: Rect,
        depth: Int,
    ): AccessibilityNodeInfo? {
        if (depth > 40) return null
        val r = Rect().also { node.getBoundsInScreen(it) }
        if (r == target && (node.isEditable || node.isClickable)) return node

        for (k in 0 until node.childCount) {
            val child = node.getChild(k) ?: continue
            findByBounds(child, target, depth + 1)?.let { return it }
            @Suppress("DEPRECATION")
            runCatching { child.recycle() }
        }
        return null
    }

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
