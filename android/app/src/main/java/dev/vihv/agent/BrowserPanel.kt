package dev.vihv.agent

import android.animation.TimeInterpolator
import android.app.Activity
import android.graphics.Canvas
import android.graphics.ColorFilter
import android.graphics.Paint
import android.graphics.Path
import android.graphics.PixelFormat
import android.graphics.Typeface
import android.graphics.drawable.Drawable
import android.graphics.drawable.GradientDrawable
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.text.TextUtils
import android.util.Log
import android.view.Gravity
import android.view.MotionEvent
import android.view.View
import android.view.ViewGroup
import android.view.WindowInsets
import android.view.animation.PathInterpolator
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import kotlin.math.abs
import kotlin.math.hypot
import kotlin.math.max
import kotlin.math.min
import kotlin.math.pow

/**
 * 浏览器看板：用户看 / 接管 agent 那只 WebView 的窗口（见 docs/08 §11）。
 *
 * 为什么是**原生** View 而不是页面里的一块 HTML：要显示的是另一只真 WebView，
 * 而 WebView 不能嵌在网页里。所以这一层盖在 UI WebView 上面，需要时把 agent
 * 那只 WebView 挂进来，关掉再摘下去 —— **同一只**，不另建。
 *
 * **长相和手感照抄页面里的查看器**（`web/src/components/Viewer.svelte`）：
 * 同一个 App 里，看沙箱网页和看 agent 的浏览器是同一类事，不该一个从右边滑进来、
 * 一个从底下升上来。所以这些数都是从那边抄来的，改那边就要改这边：
 *
 * - 顶栏：36dp + 状态栏（`--hdr`）、左边距 12dp、返回箭头 20dp（`Icon` 的
 *   `back` 那条路径）、和标题间隔 8dp、标题 16sp 中粗；内容区上沿一条分隔线
 * - 进场：从右滑入 300ms quintOut；退场：从当前位置 `240×(1-p)+80` ms quintOut；
 *   拖了一半松手弹回：280ms `cubic-bezier(0.22, 1, 0.36, 1)`（`RELEASE`）
 * - 关闭：返回箭头 / 按住顶栏往右拖（`lib/drag.ts` 同一套松手判定）/ 系统返回
 *   手势（Android 14+ 跟手：手指划到哪，面板挪到哪）
 * - 和页面一样**没有点按反馈**（见 app.css：active/hover 变体整体关掉）
 */
class BrowserPanel(private val activity: Activity) {

    companion object {
        private const val TAG = "AgentBrowserPanel"

        /** 头部状态刷新间隔（标题 / 忙不忙 / 标签变化，都是 agent 那边推动的） */
        private const val POLL_MS = 500L

        // 下面三个是 lib/drag.ts 的松手判定，单位从 CSS px 换成 dp（同一个东西）
        private const val SLOP_DP = 6f
        private const val FLING_DP_PER_MS = 0.05f
        private const val HALF = 0.4f
    }

    /** svelte/easing 的 quintOut：`1 - (1-t)^5` */
    private val quintOut = TimeInterpolator { t -> 1f - (1f - t).pow(5) }

    /** drag.ts 的 RELEASE */
    private val release = PathInterpolator(0.22f, 1f, 0.36f, 1f)

    private val density = activity.resources.displayMetrics.density
    private fun dp(v: Float): Int = (v * density).toInt()
    private fun color(id: Int) = activity.getColor(id)

    private val tick = Handler(Looper.getMainLooper())

    var visible = false
        private set

    /** 面板开着时注册的系统返回回调（类型用 Any 存，低版本设备不会碰到这个类） */
    private var backCallback: Any? = null

    /** 当前标签条上的标签短名（变了才重建；只换了当前那只就原地改高亮） */
    private var chipLabels: List<String> = emptyList()

    // —— 视图 ——

    private val title = TextView(activity).apply {
        setTextColor(color(R.color.fg))
        textSize = 16f
        typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
        maxLines = 1
        ellipsize = TextUtils.TruncateAt.END
        text = "agent 的浏览器"
    }

    private val busy = TextView(activity).apply {
        setTextColor(color(R.color.muted))
        textSize = 11f
        maxLines = 1
        text = "agent 操作中"
        visibility = View.GONE
    }

    private val header = DragRow().apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        setPadding(dp(12f), 0, dp(12f), 0)
        val back = ImageView(activity).apply {
            setImageDrawable(BackArrow(color(R.color.fg)))
            // p-1 + 20 的图标 = 28 的点按区
            setPadding(dp(4f), dp(4f), dp(4f), dp(4f))
            contentDescription = "返回"
            setOnClickListener { hide() }
        }
        addView(back, LinearLayout.LayoutParams(dp(28f), dp(28f)))
        addView(
            title,
            LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f).apply {
                marginStart = dp(8f)
            },
        )
        addView(
            busy,
            LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT)
                .apply { marginStart = dp(8f) },
        )
    }

    private val chips = LinearLayout(activity).apply {
        orientation = LinearLayout.HORIZONTAL
        setPadding(dp(12f), dp(8f), dp(12f), dp(8f))
    }

    /** 标签条：多于一只标签才出现（一只的时候它没有信息量）。标签多了能横着滑 */
    private val chipScroll = HorizontalScrollView(activity).apply {
        isHorizontalScrollBarEnabled = false
        overScrollMode = View.OVER_SCROLL_NEVER
        visibility = View.GONE
        addView(chips)
    }

    /** agent 那只 WebView 挂在这里 */
    val holder = FrameLayout(activity)

    val root: FrameLayout = FrameLayout(activity).apply {
        setBackgroundColor(color(R.color.ink))
        visibility = View.GONE
        // 面板底下是 UI WebView：别让点击漏下去
        isClickable = true
    }

    init {
        val content = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            addView(View(activity).apply { setBackgroundColor(color(R.color.hairline)) },
                LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 1))
            addView(chipScroll)
            addView(holder, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
        }
        val column = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            addView(header, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(36f)))
            addView(content, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
        }
        root.addView(column, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))

        // 边到边：顶栏让开状态栏（高度 = 36dp + 状态栏，和 `--hdr` 一样），
        // 网页让开手势条
        root.setOnApplyWindowInsetsListener { _, insets ->
            applyInsets(insets)
            insets
        }
    }

    private fun applyInsets(given: WindowInsets? = null) {
        // 面板 GONE 时 rootWindowInsets 可能是 null，最后退到 Activity 的
        val i = given ?: root.rootWindowInsets ?: activity.window.decorView.rootWindowInsets ?: return
        val (top, bottom) = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val bars = i.getInsets(WindowInsets.Type.systemBars())
            bars.top to bars.bottom
        } else {
            @Suppress("DEPRECATION")
            i.systemWindowInsetTop to i.systemWindowInsetBottom
        }
        header.setPadding(header.paddingLeft, top, header.paddingRight, 0)
        header.layoutParams = header.layoutParams.apply { height = dp(36f) + top }
        holder.setPadding(0, 0, 0, bottom)
    }

    // -----------------------------------------------------------------------
    // 开 / 关
    // -----------------------------------------------------------------------

    /** 打开。没开过页面就不开（没东西可看）。 */
    fun show() {
        val browser = AgentApp.browser
        if (browser == null || !browser.hasPage()) {
            Toast.makeText(activity, "agent 还没打开过网页", Toast.LENGTH_SHORT).show()
            return
        }
        if (visible) return
        visible = true
        browser.attachTo(holder)
        applyInsets()
        refresh()
        root.animate().cancel()
        root.visibility = View.VISIBLE
        root.translationX = width()
        root.animate().translationX(0f).setDuration(300).setInterpolator(quintOut).start()
        // 刚变可见时 insets 可能还没派发过，补一次
        root.post { applyInsets() }
        registerBack()
        tick.postDelayed(poll, POLL_MS)
        Log.i(TAG, "打开浏览器看板")
    }

    /** 关上：从当前位置（可能已经被拖 / 手势挪了一段）接着滑出去。 */
    fun hide() {
        if (!visible) return
        visible = false
        tick.removeCallbacks(poll)
        unregisterBack()
        val from = progress()
        root.animate().cancel()
        root.animate()
            .translationX(width())
            .setDuration((240 * (1 - from) + 80).toLong())
            .setInterpolator(quintOut)
            .withEndAction {
                root.visibility = View.GONE
                // 摘下来：agent 接着在无头模式跑（不重载、状态不丢）
                AgentApp.browser?.detach()
            }
            .start()
        Log.i(TAG, "收起浏览器看板")
    }

    /** Activity 要没了：不播动画，直接摘。 */
    fun destroy() {
        tick.removeCallbacks(poll)
        unregisterBack()
        root.animate().cancel()
        visible = false
        AgentApp.browser?.detach()
    }

    private fun width(): Float =
        root.width.toFloat().takeIf { it > 0 } ?: activity.resources.displayMetrics.widthPixels.toFloat()

    /** 0 = 原位，1 = 完全滑出去 */
    private fun progress(): Float = (root.translationX / width()).coerceIn(0f, 1f)

    /** 跟手：拖拽和返回手势共用 */
    private fun follow(p: Float) {
        root.animate().cancel()
        root.translationX = p.coerceIn(0f, 1f) * width()
    }

    /** 松手没关：弹回原位 */
    private fun settle() {
        root.animate().cancel()
        root.animate().translationX(0f).setDuration(280).setInterpolator(release).start()
    }

    // -----------------------------------------------------------------------
    // 头部状态 + 标签条
    // -----------------------------------------------------------------------

    private val poll = object : Runnable {
        override fun run() {
            if (!visible) return
            refresh()
            tick.postDelayed(this, POLL_MS)
        }
    }

    private fun refresh() {
        val st = AgentApp.browser?.pageState() ?: return
        title.text = st.title.ifBlank { st.url.ifBlank { "agent 的浏览器" } }
        busy.visibility = if (st.busy) View.VISIBLE else View.GONE
        syncChips(st.labels, st.currentIdx)
    }

    /**
     * 标签条。标签短名变了（开 / 关 / 跳到别的站）才重建；只是当前那只换了，就
     * **原地改高亮** —— 原来每次都 removeAllViews 重建、而且点完要等下一拍轮询
     * （最多 500ms）才重画，页面已经切过去了标签条还停在原处，看着就是卡。
     */
    private fun syncChips(labels: List<String>, current: Int) {
        if (labels.size <= 1) {
            chipScroll.visibility = View.GONE
            chipLabels = labels
            chips.removeAllViews()
            return
        }
        chipScroll.visibility = View.VISIBLE
        if (labels != chipLabels) {
            chipLabels = labels
            chips.removeAllViews()
            labels.forEachIndexed { i, label ->
                val chip = TextView(activity).apply {
                    text = "${i + 1} $label"
                    textSize = 13f
                    maxLines = 1
                    gravity = Gravity.CENTER
                    setPadding(dp(12f), 0, dp(12f), 0)
                    setOnClickListener { select(i) }
                }
                chips.addView(
                    chip,
                    LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, dp(28f))
                        .apply { if (i > 0) marginStart = dp(6f) },
                )
            }
        }
        for (i in 0 until chips.childCount) {
            val on = i == current
            (chips.getChildAt(i) as TextView).apply {
                setTextColor(color(if (on) R.color.fg else R.color.muted))
                // rounded-full bg-control（页面里药丸的样子）
                background = if (on) {
                    GradientDrawable().apply {
                        cornerRadius = dp(14f).toFloat()
                        setColor(color(R.color.control))
                    }
                } else {
                    null
                }
            }
        }
    }

    private fun select(i: Int) {
        val browser = AgentApp.browser ?: return
        if (!browser.selectTabFromUi(i)) {
            // 动作锁在 agent 手里：这时换标签，它的下一步会落在错的页上
            Toast.makeText(activity, "agent 正在操作这个页面，等它停下再切", Toast.LENGTH_SHORT).show()
            return
        }
        // 页面已经切过去了，标签条和标题立刻跟上，不等下一拍轮询
        refresh()
    }

    // -----------------------------------------------------------------------
    // 系统返回（Android 13+；14+ 有跟手进度）
    // -----------------------------------------------------------------------

    private fun registerBack() {
        if (backCallback != null || Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        val cb: Any = if (Build.VERSION.SDK_INT >= 34) {
            // 和页面覆盖层一样：手势进度直接当位移（见 web/src/lib/overlay.ts）
            object : android.window.OnBackAnimationCallback {
                override fun onBackStarted(backEvent: android.window.BackEvent) = follow(0f)
                override fun onBackProgressed(backEvent: android.window.BackEvent) =
                    follow(backEvent.progress)
                override fun onBackCancelled() = settle()
                override fun onBackInvoked() = hide()
            }
        } else {
            object : android.window.OnBackInvokedCallback {
                override fun onBackInvoked() = hide()
            }
        }
        @Suppress("NewApi")
        activity.onBackInvokedDispatcher.registerOnBackInvokedCallback(
            // 比页面那层（PRIORITY_DEFAULT）高：看板盖在页面上面
            android.window.OnBackInvokedDispatcher.PRIORITY_OVERLAY,
            cb as android.window.OnBackInvokedCallback,
        )
        backCallback = cb
    }

    private fun unregisterBack() {
        val cb = backCallback ?: return
        backCallback = null
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        @Suppress("NewApi")
        activity.onBackInvokedDispatcher.unregisterOnBackInvokedCallback(
            cb as android.window.OnBackInvokedCallback,
        )
    }

    // -----------------------------------------------------------------------
    // 按住顶栏往右拖关闭（lib/drag.ts 的 dismissable，axis x / dir +1）
    // -----------------------------------------------------------------------

    /**
     * 顶栏。拖拽判定放在 `onInterceptTouchEvent`：手指落在返回箭头上也能拖 ——
     * 认定是横着往右拖之后才把事件从子 View 手里拿过来，没认定之前点击照常。
     */
    private inner class DragRow : LinearLayout(activity) {
        private var x0 = 0f
        private var y0 = 0f
        private var base = 0f
        private var dragging = false
        private var undecided = false
        /** 最近 100ms 的 (时间, 位移 px)，算松手速度 */
        private val samples = ArrayDeque<Pair<Long, Float>>()

        override fun onInterceptTouchEvent(e: MotionEvent): Boolean {
            when (e.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    x0 = e.rawX
                    y0 = e.rawY
                    dragging = false
                    undecided = true
                    samples.clear()
                }
                MotionEvent.ACTION_MOVE -> if (undecided) decide(e)
            }
            return dragging
        }

        override fun onTouchEvent(e: MotionEvent): Boolean {
            when (e.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    x0 = e.rawX
                    y0 = e.rawY
                    undecided = true
                    samples.clear()
                    return true
                }
                MotionEvent.ACTION_MOVE -> {
                    if (undecided) decide(e)
                    if (dragging) move(e)
                }
                MotionEvent.ACTION_UP -> if (dragging) end()
                MotionEvent.ACTION_CANCEL -> if (dragging) {
                    dragging = false
                    settle()
                }
            }
            return true
        }

        private fun decide(e: MotionEvent) {
            val dx = e.rawX - x0
            val dy = e.rawY - y0
            if (hypot(dx, dy) < SLOP_DP * density) return
            undecided = false
            if (abs(dx) > abs(dy) && dx > 0) {
                dragging = true
                base = dx
                parent?.requestDisallowInterceptTouchEvent(true)
            }
        }

        private fun move(e: MotionEvent) {
            val px = max(0f, e.rawX - x0 - base)
            samples.addLast(e.eventTime to px)
            while (samples.size > 2 && e.eventTime - samples.first().first > 100) samples.removeFirst()
            follow(min(1f, px / width()))
        }

        private fun end() {
            dragging = false
            var v = 0f
            if (samples.size >= 2) {
                val (t1, a) = samples.first()
                val (t2, b) = samples.last()
                v = (b - a) / max(1L, t2 - t1) / density
            }
            val close = when {
                v > FLING_DP_PER_MS -> true
                v < -FLING_DP_PER_MS -> false
                else -> progress() > HALF
            }
            if (close) hide() else settle()
        }
    }

    /** 返回箭头：`Icon.svelte` 的 `back`（viewBox 20，`M12.5 4.5 7 10l5.5 5.5`，描边 1.8） */
    private class BackArrow(color: Int) : Drawable() {
        private val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeCap = Paint.Cap.ROUND
            strokeJoin = Paint.Join.ROUND
            this.color = color
        }
        private val path = Path()

        override fun draw(canvas: Canvas) {
            val b = bounds
            val s = min(b.width(), b.height()) / 20f
            paint.strokeWidth = 1.8f * s
            path.reset()
            path.moveTo(b.left + 12.5f * s, b.top + 4.5f * s)
            path.lineTo(b.left + 7f * s, b.top + 10f * s)
            path.lineTo(b.left + 12.5f * s, b.top + 15.5f * s)
            canvas.drawPath(path, paint)
        }

        override fun setAlpha(alpha: Int) {
            paint.alpha = alpha
        }

        override fun setColorFilter(colorFilter: ColorFilter?) {
            paint.colorFilter = colorFilter
        }

        @Deprecated("Drawable 的老接口，平台还要求实现")
        override fun getOpacity(): Int = PixelFormat.TRANSLUCENT
    }
}
