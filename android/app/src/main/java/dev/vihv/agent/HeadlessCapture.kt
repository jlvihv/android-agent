package dev.vihv.agent

import android.app.Presentation
import android.content.Context
import android.graphics.Bitmap
import android.graphics.Color
import android.graphics.PixelFormat
import android.graphics.drawable.ColorDrawable
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.Image
import android.media.ImageReader
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.view.WindowManager
import android.webkit.WebView
import android.widget.FrameLayout
import java.nio.ByteBuffer
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * 截图里要有 WebGL / `<video>`，就得让 **GPU 合成器**画一帧 —— 而不是
 * `View.draw(软件 Canvas)`。
 *
 * 为什么软件绘制不行：Chromium 收到软件绘制会在 CPU 上重新栅格化，而 GPU
 * 才有的层（WebGL canvas、video）它拿不到，结果是空白（实测：同一个页面在
 * 屏幕上有画面，截图里那块是白的）。
 *
 * 麻烦在于：**不挂窗口的 View 没有 surface**，也没有屏幕像素可读
 * （`PixelCopy` 无从谈起）。所以这里给它临时搭一个 surface：
 *
 * ```
 * ImageReader(RGBA) ──surface──> VirtualDisplay ──> Presentation 窗口 ──> 挂 WebView
 *        └──────────────────── 读回一帧 ◄── GPU 正常合成 ──┘
 * ```
 *
 * 全是**本进程私有**的：`OWN_CONTENT_ONLY` 的虚拟显示、不需要权限、用户看不见。
 * 窗口设成 NOT_FOCUSABLE | NOT_TOUCHABLE —— 页面里的输入框绝不能把输入法弹到
 * 用户的真屏幕上。
 *
 * 成本控制（都很实际）：
 * - 第一次截图才建，之后空闲 [IDLE_RELEASE_MS] 就释放：空闲的 App 不付渲染/
 *   内存/电量的钱，而且页面不会在后台一直动画。
 * - 尺寸超过 [MAX_EDGE_PX]（整页截图必然超过）直接不走这条路，退回软件绘制。
 * - 任何一步失败都返回 null，调用方退回软件绘制 —— 最坏情况就是以前的行为。
 * - 连续失败 [MAX_FAILURES] 次就在本进程内停用，不让一台不支持虚拟显示的设备
 *   每次都白试一遍。
 *
 * 线程：`capture` 在**调用线程**（Rust 的 spawn_blocking 线程）上跑，中间会等；
 * 碰 View/窗口/WebView 的都在主线程。两者用 [CountDownLatch] 和几个 volatile
 * 计数器交接 —— 唯一需要小心的是别在主线程上等主线程（那会自己等自己）。
 */
object HeadlessCapture {

    private const val TAG = "HeadlessCapture"

    /** 空闲这么久就把虚拟显示放掉（下次再建） */
    private const val IDLE_RELEASE_MS = 12_000L

    /** 单边上限。整页截图（几万像素高）留走软件绘制那条路 */
    private const val MAX_EDGE_PX = 4_096

    /** 等 WebView 视觉状态回调的上限 */
    private const val VISUAL_STATE_TIMEOUT_MS = 1_500L

    /** 一帧算"定了"：至少来 [MIN_FRAMES] 帧，且最近 [QUIET_MS] 没有新帧 */
    private const val MIN_FRAMES = 2
    private const val QUIET_MS = 150L
    private const val SETTLE_TIMEOUT_MS = 900L

    /** 连续失败这么多次就不再试了（进程内） */
    private const val MAX_FAILURES = 3

    private val main = Handler(Looper.getMainLooper())

    /** 本进程已经放弃 GPU 截图的原因（不为 null 就一直走软件绘制） */
    @Volatile
    private var disabled: String? = null

    // ---- 下面这些只在主线程碰 ----
    private var reader: ImageReader? = null
    private var display: VirtualDisplay? = null
    private var presentation: Presentation? = null
    private var container: FrameLayout? = null
    private var hostW = 0
    private var hostH = 0
    private var latest: Image? = null
    private var failures = 0

    // 主线程写，调用线程读 → volatile
    @Volatile
    private var frameCount = 0L

    @Volatile
    private var lastFrameAt = 0L

    private val releaseTask = Runnable { release("空闲") }

    /**
     * 截一帧。成功给 Bitmap，不可用/失败给 null（调用方退回软件绘制）。
     *
     * **不能在主线程调** —— 里面要等合成器出帧。
     */
    fun capture(app: Context, web: WebView, w: Int, h: Int): Bitmap? {
        if (disabled != null) return null
        if (w <= 0 || h <= 0 || w > MAX_EDGE_PX || h > MAX_EDGE_PX) return null
        // 面板正挂着它：那它本来就挂在真窗口上，不该被我们挪走（挪了用户
        // 看着的那块会闪白）。这种情况退回软件绘制。
        if (onMain { web.parent != null } == true) return null

        val t0 = SystemClock.elapsedRealtime()
        var img: Image? = null
        // 滚动条的原值（异常路径下也要能恢复）
        var vBar = true
        var hBar = true
        var moved = false
        try {
            val host = onMain { ensureHost(app, w, h) } ?: return null
            val tHost = SystemClock.elapsedRealtime()

            // 滚动条会淡入并被烤进画面（软件绘制从不画它）→ 截的时候关掉
            vBar = onMain { web.isVerticalScrollBarEnabled } ?: true
            hBar = onMain { web.isHorizontalScrollBarEnabled } ?: true
            onMain {
                web.isVerticalScrollBarEnabled = false
                web.isHorizontalScrollBarEnabled = false
                host.addView(web, FrameLayout.LayoutParams(w, h))
            }
            moved = true

            val ready = awaitVisualState(web)
            val start = frameCount
            onMain { web.invalidate() }
            val settled = awaitFrames(start)
            val frames = frameCount - start

            if (frames < 1) {
                Log.w(TAG, "一帧都没来（ready=$ready ${SystemClock.elapsedRealtime() - t0}ms）→ 退回软件绘制")
                return null
            }
            // 出一帧就够了：页面永远在动（动画）时它不会安静，但任意一帧都是
            // 真实画面。安静只是"更完整"（重排/栅格化补齐）的判据。
            val take = onMain {
                val i = latest
                latest = null
                i
            }
            onMain { host.removeView(web) }
            img = take ?: return null

            // 复制到 Bitmap：~w*h*4 字节，放在调用线程上做（不是在主线程）
            val bmp = toBitmap(img, w, h)
            if (bmp != null) {
                failures = 0
                Log.i(
                    TAG,
                    "GPU 截图 ${w}x$h 总 ${SystemClock.elapsedRealtime() - t0}ms " +
                        "（建宿主 ${tHost - t0}ms，帧 $frames，安静=$settled）",
                )
            }
            return bmp
        } catch (t: Throwable) {
            Log.w(TAG, "GPU 截图失败：${t.javaClass.simpleName}: ${t.message}")
            return null
        } finally {
            runCatching { img?.close() }
            if (moved) {
                onMain {
                    web.isVerticalScrollBarEnabled = vBar
                    web.isHorizontalScrollBarEnabled = hBar
                }
                // 万一还挂在宿主上（异常路径），摘下来
                onMain { container?.let { c -> if (web.parent === c) c.removeView(web) } }
            }
            main.postDelayed(releaseTask, IDLE_RELEASE_MS)
        }
    }

    /**
     * 等 WebView 当前的视觉状态进到下一帧。
     *
     * `postVisualStateCallback` 是官方给的"重挂之后别拿旧帧/空帧"的手段。
     * 有上限：渲染进程卡死的话，截图不能跟着卡死。
     */
    private fun awaitVisualState(web: WebView): Boolean {
        val done = CountDownLatch(1)
        val id = SystemClock.elapsedRealtimeNanos()
        onMain {
            web.postVisualStateCallback(
                id,
                object : WebView.VisualStateCallback() {
                    override fun onComplete(requestId: Long) {
                        if (requestId == id) done.countDown()
                    }
                },
            )
        }
        return done.await(VISUAL_STATE_TIMEOUT_MS, TimeUnit.MILLISECONDS)
    }

    /** 等至少 [MIN_FRAMES] 帧、且最近 [QUIET_MS] 没新帧。超时不算失败（动画页静不下来） */
    private fun awaitFrames(start: Long): Boolean {
        val deadline = System.currentTimeMillis() + SETTLE_TIMEOUT_MS
        while (System.currentTimeMillis() < deadline) {
            val n = frameCount
            if (n >= start + MIN_FRAMES && System.currentTimeMillis() - lastFrameAt >= QUIET_MS) {
                return true
            }
            Thread.sleep(8)
        }
        return false
    }

    /** 建（或复用）宿主。**主线程** */
    private fun ensureHost(context: Context, w: Int, h: Int): FrameLayout? {
        container?.let { if (presentation != null && hostW == w && hostH == h) return it }
        release("换尺寸")

        val app = context.applicationContext
        var r: ImageReader? = null
        var vd: VirtualDisplay? = null
        try {
            val dm = app.getSystemService(DisplayManager::class.java) ?: return fail("没有 DisplayManager")
            r = ImageReader.newInstance(w, h, PixelFormat.RGBA_8888, 3)
            // 只留最新一帧：ImageReader 的队列满了之后就**不再回调**了
            r.setOnImageAvailableListener({ rr ->
                val img = runCatching { rr.acquireLatestImage() }.getOrNull() ?: return@setOnImageAvailableListener
                latest?.close()
                latest = img
                frameCount++
                lastFrameAt = SystemClock.elapsedRealtime()
            }, main)

            // 不带 PUBLIC：这是本进程私有的显示，不需要权限、用户看不见
            vd = dm.createVirtualDisplay(
                "agent-headless-browser",
                w,
                h,
                app.resources.displayMetrics.densityDpi,
                r.surface,
                DisplayManager.VIRTUAL_DISPLAY_FLAG_OWN_CONTENT_ONLY,
            ) ?: run { r.close(); return fail("createVirtualDisplay 返回 null") }

            val p = Presentation(app, vd.display)
            p.window?.apply {
                addFlags(
                    WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or
                        WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE,
                )
                setBackgroundDrawable(ColorDrawable(Color.WHITE))
            }
            val c = FrameLayout(p.context)
            p.setContentView(c)
            p.show()

            reader = r
            display = vd
            presentation = p
            container = c
            hostW = w
            hostH = h
            Log.i(TAG, "宿主已建 ${w}×$h（dpi=${app.resources.displayMetrics.densityDpi}）")
            return c
        } catch (t: Throwable) {
            runCatching { vd?.release() }
            runCatching { r?.close() }
            return fail("建宿主失败：${t.javaClass.simpleName}: ${t.message}")
        }
    }

    private fun fail(reason: String): FrameLayout? {
        failures++
        Log.w(TAG, "$reason（第 $failures/$MAX_FAILURES 次）")
        if (failures >= MAX_FAILURES) {
            disabled = reason
            Log.w(TAG, "本进程不再走 GPU 截图：$reason")
        }
        return null
    }

    /** 拆干净。**主线程** */
    private fun release(why: String) {
        if (presentation == null && display == null && reader == null) return
        main.removeCallbacks(releaseTask)
        container?.removeAllViews()
        runCatching { presentation?.dismiss() }
        runCatching { display?.release() }
        runCatching { latest?.close() }
        latest = null
        runCatching { reader?.close() }
        presentation = null
        display = null
        reader = null
        container = null
        Log.i(TAG, "宿主已释放（$why）")
    }

    /**
     * RGBA_8888 的 [Image] → ARGB_8888 的 Bitmap。
     *
     * 不能整块 `copyPixelsFromBuffer`：plane 的**行可能有padding**
     * （`rowStride > w*4`），整块拷会一行一行地错位（画面斜着花）。所以按行重排。
     */
    private fun toBitmap(image: Image, w: Int, h: Int): Bitmap? = runCatching {
        val plane = image.planes[0]
        val src = plane.buffer.duplicate().apply { rewind() }
        val rowStride = plane.rowStride
        val pixelStride = plane.pixelStride
        val rowBytes = w * pixelStride
        val packed = ByteBuffer.allocateDirect(rowBytes * h)
        val row = ByteArray(rowBytes)
        for (y in 0 until h) {
            src.position(y * rowStride)
            src.get(row, 0, rowBytes)
            packed.put(row)
        }
        packed.rewind()
        Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888).apply {
            copyPixelsFromBuffer(packed)
        }
    }.onFailure { Log.w(TAG, "转 Bitmap 失败", it) }.getOrNull()

    /** 主线程往返：post 过去、等它做完 */
    private fun <T> onMain(timeoutMs: Long = 4_000L, block: () -> T): T? {
        if (Looper.myLooper() == Looper.getMainLooper()) {
            // 已经在主线程：直接做（主线程里再 post + 等就是自己等自己）
            return runCatching(block).getOrNull()
        }
        val latch = CountDownLatch(1)
        var out: T? = null
        main.post {
            try {
                out = block()
            } catch (t: Throwable) {
                Log.w(TAG, "主线程操作失败", t)
            } finally {
                latch.countDown()
            }
        }
        return if (latch.await(timeoutMs, TimeUnit.MILLISECONDS)) out else null
    }
}
