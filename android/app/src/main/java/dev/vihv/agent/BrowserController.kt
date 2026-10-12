package dev.vihv.agent

import android.app.Application
import android.graphics.Bitmap
import android.graphics.Canvas
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.View
import android.webkit.CookieManager
import android.webkit.JsPromptResult
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import org.json.JSONArray
import org.json.JSONObject
import uniffi.agent_core.BrowserHost
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.locks.ReentrantLock
import kotlin.concurrent.withLock

/**
 * agent 的浏览器：**一个不挂在任何窗口上的真 WebView**。
 *
 * 决策见 `docs/08-browser.md`。要点：
 *
 * - **不挂窗口**才是关键，不是"要不要让用户看见"。WebView 不需要可见、不需要
 *   屏幕亮着就能加载页面、执行注入的 JS。反过来，如果和用户共用一个可见的
 *   WebView，用户一操作就把 agent 的页面导走了 —— 所以两者**绝不共用**。
 * - 用户之后想看 agent 在干什么，就把这个 WebView 临时挂进一个 sheet 显示
 *   （`addView` / `removeView`），看完摘掉，agent 照跑。这一步现在没做。
 * - 不挂窗口的代价：View 没有被父容器测量过，尺寸是 0 —— 必须在创建时
 *   **自己 `measure()` + `layout()`**，否则页面视口是 0×0，懒加载不触发、
 *   移动版布局全错。见 [ensureWeb]。
 *
 * 线程模型：WebView 只能在主线程碰，而这个类的方法是在 Rust 的
 * `spawn_blocking` 线程上调用的。所以每次碰 WebView 都是
 * 「post 到主线程 + 在调用线程上等」([onMain] / [eval])。**别在 [onMain]
 * 的 block 里再调 [onMain] 或 [eval]** —— 主线程会自己等自己，直接死锁。
 *
 * 一次只能跑一个动作（[lock]）：两个动作交叉执行会让 `navigate A → click`
 * 点到别的页面上。
 */
class BrowserController(private val app: Application) : BrowserHost {

    companion object {
        private const val TAG = "AgentBrowser"

        /** 一次 `evaluateJavascript` 的回调预算。健康的页面毫秒级就回来了 */
        private const val JS_TIMEOUT_MS = 15_000L

        /** navigate 等 `onPageFinished` 的上限。等不到不算失败（SPA 经常不触发） */
        private const val NAV_TIMEOUT_MS = 25_000L

        /** 页面停止后额外等一会儿再读 DOM：JS 渲染（SPA）大多在这一小段里完成 */
        private const val SETTLE_MS = 400L

        /** 骨架树给模型的文本上限（它会进历史，按钱算） */
        private const val BACKBONE_MAX_CHARS = 12_000
        private const val BACKBONE_MAX_NODES = 220

        /** 正文上限 */
        private const val TEXT_MAX_CHARS = 12_000

        /** 截图目录（沙箱里的路径，`read` 那个工具能直接读） */
        private const val SHOT_GUEST_DIR = "/root/.browser"

        /** 截图压缩质量。截图进的是模型上下文，不是相册 */
        private const val SHOT_QUALITY = 80
    }

    /** 串行化动作。见类注释 */
    private val lock = ReentrantLock()

    private val main = Handler(Looper.getMainLooper())

    /** 主线程独占。其它线程只读（构建完成后指针就不再变，除了换新的）*/
    @Volatile
    private var web: WebView? = null

    /**
     * 渲染进程死了（`onRenderProcessGone`）或 JS 一直不回调。
     * 已死的 WebView 调什么都不会有反应，只能整只换掉（[ensureWeb]）。
     */
    @Volatile
    private var dead = false

    /**
     * 主文档的加载错误（onReceivedError 给的），navigate 会读出来报给模型
     */
    @Volatile
    private var lastError: String? = null

    // 页面加载的进度计数。为什么不用 CountDownLatch：一次动作（点击）**可能**
    // 触发导航，也可能不触发，而导航还可能重定向好几次（多次 start/finish）。
    // 计数器比一次性闩好描述这类事：
    //
    //   navStarted  页每次开始加载 +1（`onPageStarted`）
    //   navFinished 页每次加载结束 +1（`onPageFinished` / 主文档出错 / 渲染进程死）
    //
    // 判据“一次加载跑完了” = `started > 快照 && finished >= started`。
    private val navStarted = AtomicInteger()
    private val navFinished = AtomicInteger()

    private fun navCounters(): Pair<Int, Int> = navStarted.get() to navFinished.get()

    /** 等加载结束。`started0` 是动作前的 `navStarted` 快照 */
    private fun awaitLoad(started0: Int, maxMs: Long): Boolean {
        val deadline = System.currentTimeMillis() + maxMs
        var sawStart = false
        while (System.currentTimeMillis() < deadline) {
            val (s, f) = navCounters()
            if (s > started0) sawStart = true
            if (sawStart && f >= s) return true
            Thread.sleep(60)
        }
        return false
    }

    /**
     * 动作之后等页面安定：先看它跳没跳。
     *
     * 只 sleep 固定时间的话读到的还是**上一页**的 url/title，模型会以为没跳
     * （实测踩过：点了一个外站链接，返回的 url 还是原来那个）。
     */
    private fun settleAfterAction(started0: Int) {
        // 最多等 1.2s 看有没有开始加载（点击后跳页很快就会触发）
        val deadline = System.currentTimeMillis() + 1200
        var navigating = false
        while (System.currentTimeMillis() < deadline) {
            if (navCounters().first > started0) {
                navigating = true
                break
            }
            Thread.sleep(60)
        }
        if (navigating) awaitLoad(started0, 20_000)
        Thread.sleep(SETTLE_MS)
    }

    /** 页面弹过的 alert/confirm/prompt。它们被我们吞掉了，得让模型知道 */
    private val dialogs = ArrayDeque<String>()

    // -----------------------------------------------------------------------
    // 入口
    // -----------------------------------------------------------------------

    /**
     * 由 Rust 从 `spawn_blocking` 线程调用。返回结果 JSON。
     *
     * 约定形状：`{ok, text, url, title, image?}`。**动作失败不抛异常** ——
     * 抛出去只会变成一句"浏览器调用失败"，而模型需要知道的是"元素没找到"
     * 这种能自己纠正的信息。
     */
    override fun execute(actionJson: String): String {
        val action = try {
            JSONObject(actionJson)
        } catch (t: Throwable) {
            return err("参数不是合法 JSON: ${t.message}")
        }

        lock.withLock {
            return try {
                dispatch(action)
            } catch (t: Throwable) {
                Log.w(TAG, "动作失败: $actionJson", t)
                err(t.message ?: t.toString())
            }
        }
    }

    private fun dispatch(a: JSONObject): String {
        val name = a.optString("action")
        Log.i(TAG, "action=$name ${a.toString().take(200)}")

        lastError = null

        val out: JSONObject = when (name) {
            "navigate" -> navigate(a.optString("url"))
            "get_backbone" -> getBackbone(a.optInt("max_depth", 5))
            "click" -> click(a)
            "type" -> type(a.optString("selector"), a.optString("text"))
            "get_text" -> getText(a.optString("selector").ifEmpty { null })
            "scroll" -> scroll(a)
            "screenshot" -> screenshot()
            "back" -> goBack()
            "js" -> runJs(a.optString("script"))
            else -> return err("未知动作: $name")
        }

        // 吞掉的对话框每轮报一次，然后清空（模型知道被拦了什么就够）
        val d = drainDialogs()
        if (d != null) out.put("text", out.optString("text") + "\n[页面弹窗已自动关闭] $d")
        return out.toString()
    }

    // -----------------------------------------------------------------------
    // WebView 的创建 / 维护
    // -----------------------------------------------------------------------

    /**
     * 拿一个能用的 WebView，必要时建一个（或重建）。
     *
     * **尺寸是自己给的**：这个 View 永远不会被父容器测量，所以必须
     * `measure()` + `layout()` 一次。用手机屏幕的物理像素 —— 于是页面看到的
     * 视口（`innerWidth`）就是 `屏幕宽 / density`，和用户自己在这台手机上
     * 开 Chrome 看到的一致。
     */
    private fun ensureWeb(): WebView {
        val got = onMain {
            val cur = web
            if (cur != null && !dead) {
                cur
            } else {
                if (cur != null) {
                    Log.w(TAG, "丢弃坏掉的 WebView，重开一只")
                    runCatching { cur.destroy() }
                    web = null
                }
                dead = false
                val w = buildWebView()
                web = w
                w
            }
        }
        if (got is WebView) return got
        throw IllegalStateException("WebView 建不出来（上一行日志里有原因）")
    }

    private fun buildWebView(): WebView {
        val dm = app.resources.displayMetrics
        val w = dm.widthPixels.coerceAtLeast(320)
        val h = dm.heightPixels.coerceAtLeast(640)

        val v = WebView(app)
        v.settings.apply {
            javaScriptEnabled = true
            domStorageEnabled = true
            @Suppress("DEPRECATION")
            databaseEnabled = true
            // 不缩放的手机版视口：和用户自己开浏览器一致
            useWideViewPort = true
            loadWithOverviewMode = true
            setSupportZoom(false)
            builtInZoomControls = false
            mediaPlaybackRequiresUserGesture = false
            cacheMode = WebSettings.LOAD_DEFAULT
            // 比 WebView 的默认（NEVER_ALLOW）更接近 Chrome：https 页面里的
            // http 图片/脚本照旧能出来。WebView 默认会把它们全丢掉，看起来
            // 就像"页面缺了一块"，而模型无从判断
            mixedContentMode = WebSettings.MIXED_CONTENT_COMPATIBILITY_MODE
            // 页面不能借我们的身份读本地文件
            allowFileAccess = false
            allowContentAccess = false
            // 弹窗（target=_blank / window.open）不另开窗口：见 installTargetFix
            setSupportMultipleWindows(false)
        }

        // 第三方 cookie 要显式打开（WebView 默认关）。登录态、OAuth 回调、
        // hCaptcha/Turnstile 这类"验证控件在跨域 iframe 里、靠 Set-Cookie
        // 把结果回写给父页面"的流程，关着就直接挂
        runCatching { CookieManager.getInstance().setAcceptThirdPartyCookies(v, true) }

        v.webViewClient = object : WebViewClient() {
            override fun shouldOverrideUrlLoading(
                view: WebView?,
                request: WebResourceRequest?,
            ): Boolean {
                val uri = request?.url ?: return true
                val scheme = uri.scheme?.lowercase()
                // 只走 http(s)。`intent://`、`market://`、`tel:`、`file://`
                // 一律挡掉 —— 页面不该能拉起别的 App，更不该读本地文件
                if (scheme != "http" && scheme != "https") {
                    Log.i(TAG, "挡下跳转: $uri")
                    return true
                }
                return false
            }

            override fun onPageFinished(view: WebView?, url: String?) {
                navFinished.incrementAndGet()
            }

            override fun onReceivedError(
                view: WebView?,
                request: WebResourceRequest?,
                error: android.webkit.WebResourceError?,
            ) {
                // 只记主文档：子资源失败（一张图 404）不该让整个 navigate 变红
                if (request?.isForMainFrame == true) {
                    lastError = "加载失败: ${error?.description}（${error?.errorCode}）"
                    navFinished.incrementAndGet()
                }
            }

            /**
             * 渲染进程被系统回收。**必须返回 true** —— 返回 false 的话
             * Android 会把整个 App 进程杀掉。返回 true 之后这只 WebView
             * 已经废了，下一次动作由 [ensureWeb] 换一只新的。
             */
            override fun onRenderProcessGone(
                view: WebView?,
                detail: android.webkit.RenderProcessGoneDetail?,
            ): Boolean {
                Log.w(TAG, "渲染进程没了（crashed=${detail?.didCrash()}），下次换一只")
                dead = true
                navFinished.incrementAndGet()
                return true
            }

            /** 每页开始加载时把 target=_blank 掰成 _self（见 installTargetFix） */
            override fun onPageStarted(view: WebView?, url: String?, favicon: android.graphics.Bitmap?) {
                navStarted.incrementAndGet()
                view?.evaluateJavascript(TARGET_FIX_JS, null)
            }
        }

        v.webChromeClient = object : WebChromeClient() {
            // alert/confirm/prompt 会被主线程弹窗卡住整个渲染进程（而且我们
            // 用的 Application context 根本没有 Activity 窗口）。全部吞掉，
            // 记下来下一轮告诉模型
            override fun onJsAlert(
                view: WebView?, url: String?, message: String?, result: android.webkit.JsResult?,
            ): Boolean {
                record("alert: $message")
                result?.confirm()
                return true
            }

            override fun onJsConfirm(
                view: WebView?, url: String?, message: String?, result: android.webkit.JsResult?,
            ): Boolean {
                record("confirm: $message（按了确定）")
                result?.confirm()
                return true
            }

            override fun onJsPrompt(
                view: WebView?, url: String?, message: String?, defaultValue: String?,
                result: JsPromptResult?,
            ): Boolean {
                record("prompt: $message（空回答）")
                result?.confirm("")
                return true
            }

            /** 页面问"确定要离开吗" —— 不然导航会永远停在这里 */
            override fun onJsBeforeUnload(
                view: WebView?, url: String?, message: String?, result: android.webkit.JsResult?,
            ): Boolean {
                result?.confirm()
                return true
            }

            /** 只报警告/错误：重页面（CNBC 那种）每屏几百条 console，
             *  全部 Log.d 会把主线程拖住 —— 而主线程还要跑我们注入的动作。*/
            override fun onConsoleMessage(m: android.webkit.ConsoleMessage?): Boolean {
                val lv = m?.messageLevel()
                if (lv == android.webkit.ConsoleMessage.MessageLevel.ERROR ||
                    lv == android.webkit.ConsoleMessage.MessageLevel.WARNING
                ) {
                    Log.d(TAG, "[page:$lv] ${m?.message()?.take(300)} @${m?.lineNumber()}")
                }
                return true
            }
        }

        // 关键一步：给它一个尺寸，否则视口是 0×0
        v.measure(
            View.MeasureSpec.makeMeasureSpec(w, View.MeasureSpec.EXACTLY),
            View.MeasureSpec.makeMeasureSpec(h, View.MeasureSpec.EXACTLY),
        )
        v.layout(0, 0, w, h)
        Log.i(TAG, "WebView ${w}×${h}px（视口 ${w / dm.density}×${h / dm.density} CSS px）")
        return v
    }

    private fun record(msg: String) {
        synchronized(dialogs) {
            if (dialogs.size < 5) dialogs.addLast(msg)
        }
    }

    private fun drainDialogs(): String? = synchronized(dialogs) {
        if (dialogs.isEmpty()) return null
        val all = dialogs.joinToString("；")
        dialogs.clear()
        all
    }

    // -----------------------------------------------------------------------
    // 动作
    // -----------------------------------------------------------------------

    private fun navigate(urlRaw: String?): JSONObject {
        val url = (urlRaw ?: "").trim()
        if (url.isEmpty()) return errObj("缺少参数 url")
        // 没写协议头按 https 补；只认 http(s)，挡掉 file:// / javascript: 之类
        val target = if (url.contains("://")) url else "https://$url"
        val scheme = runCatching { android.net.Uri.parse(target).scheme?.lowercase() }.getOrNull()
        if (scheme != "http" && scheme != "https") return errObj("只支持 http/https：$url")

        val w = ensureWeb()
        val snap = navCounters().first
        onMain { w.loadUrl(target) }
        val finished = awaitLoad(snap, NAV_TIMEOUT_MS)
        if (!finished) {
            // SPA 常常不触发 onPageFinished；不是失败，把能读到的如实报出来
            Log.i(TAG, "等 onPageFinished 超时，照样往下读")
        }
        // JS 渲染出来的内容大多在这一小段里出现
        Thread.sleep(SETTLE_MS)

        val info = pageInfo()
        val title = info?.optString("title").orEmpty()
        val realUrl = info?.optString("url").orEmpty().ifEmpty { target }
        val text = buildString {
            append("已打开 ")
            append(realUrl)
            if (title.isNotEmpty()) append("\n标题：$title")
            if (!finished) append("\n（页面可能还在加载）")
            lastError?.let { append("\n$it") }
        }
        return ok(text, realUrl, title)
    }

    private fun pageInfo(): JSONObject? = runCatching {
        JSONObject(
            eval(
                """
                (function() {
                  return JSON.stringify({
                    url: location.href,
                    title: document.title,
                    ready: document.readyState,
                    scrollY: Math.round(window.scrollY),
                    scrollHeight: document.documentElement.scrollHeight,
                    innerWidth: window.innerWidth,
                    innerHeight: window.innerHeight
                  });
                })()
                """.trimIndent(),
            ),
        )
    }.getOrNull()

    private fun getBackbone(maxDepth: Int): JSONObject {
        ensureWeb()
        val raw = eval(backboneJs(maxDepth.coerceIn(1, 10)), timeout = JS_TIMEOUT_MS)
        val json = runCatching { JSONObject(raw) }.getOrNull()
            ?: return errObj("读页面结构失败")
        if (json.has("error")) return errObj(json.optString("error"))

        var text = json.optString("text")
        if (text.length > BACKBONE_MAX_CHARS) {
            text = text.take(BACKBONE_MAX_CHARS) + "\n…（树太长，已截断；用 selector 缩小范围或调小 max_depth）"
        }
        val info = pageInfo()
        return ok(
            "页面结构（${json.optInt("nodes")} 个节点，深度 ${json.optInt("depth")}）：\n$text",
            info?.optString("url").orEmpty(),
            info?.optString("title").orEmpty(),
        )
    }

    private fun click(a: JSONObject): JSONObject {
        ensureWeb()
        val sel = a.optString("selector")
        val js = if (sel.isNotEmpty()) {
            clickSelJs(sel)
        } else if (a.has("x") && a.has("y")) {
            clickXyJs(a.optInt("x"), a.optInt("y"))
        } else {
            return errObj("click 需要 selector，或者 x + y")
        }
        val snap = navCounters().first
        val out = evalToJson(js) ?: return errObj("点击失败")
        out.optString("error").takeIf { it.isNotEmpty() }?.let { return errObj(it) }

        settleAfterAction(snap)
        val info = pageInfo()
        return ok(
            "已点击 <${out.optString("tag")}> ${out.optString("text").take(80)}",
            info?.optString("url").orEmpty(),
            info?.optString("title").orEmpty(),
        )
    }

    private fun type(sel: String?, text: String?): JSONObject {
        if (sel.isNullOrEmpty()) return errObj("缺少参数 selector")
        ensureWeb()
        val snap = navCounters().first
        val out = evalToJson(typeJs(sel, text.orEmpty())) ?: return errObj("输入失败")
        out.optString("error").takeIf { it.isNotEmpty() }?.let { return errObj(it) }
        settleAfterAction(snap)
        return ok("已往 ${sel} 输入 ${out.optInt("length")} 个字符")
    }

    private fun getText(sel: String?): JSONObject {
        ensureWeb()
        val out = evalToJson(getTextJs(sel)) ?: return errObj("读文本失败")
        out.optString("error").takeIf { it.isNotEmpty() }?.let { return errObj(it) }

        var text = out.optString("text")
        if (text.length > TEXT_MAX_CHARS) {
            text = text.take(TEXT_MAX_CHARS) + "\n…（已截断）"
        }
        val head = if (sel.isNullOrEmpty()) "整页正文" else "$sel 的文本"
        return ok("$head（${out.optInt("length")} 字）：\n$text")
    }

    private fun scroll(a: JSONObject): JSONObject {
        ensureWeb()
        val dir = if (a.optString("direction") == "up") "up" else "down"
        val amount = a.optInt("amount", 500).coerceIn(50, 5000)
        val out = evalToJson(scrollJs(dir, amount, a.optString("selector").ifEmpty { null }))
            ?: return errObj("滚动失败")
        out.optString("error").takeIf { it.isNotEmpty() }?.let { return errObj(it) }
        Thread.sleep(200)
        return ok(
            "已滚动 $dir $amount（scrollY=${out.optInt("scrollY")}/" +
                "${out.optInt("scrollHeight")}）",
        )
    }

    private fun goBack(): JSONObject {
        val w = ensureWeb()
        val can = onMain { w.canGoBack() } as? Boolean ?: false
        if (!can) return errObj("没有上一页")
        val snap = navCounters().first
        onMain { w.goBack() }
        awaitLoad(snap, NAV_TIMEOUT_MS)
        Thread.sleep(SETTLE_MS)
        val info = pageInfo()
        return ok(
            "已后退到 ${info?.optString("url").orEmpty()}",
            info?.optString("url").orEmpty(),
            info?.optString("title").orEmpty(),
        )
    }

    private fun runJs(script: String?): JSONObject {
        if (script.isNullOrBlank()) return errObj("缺少参数 script")
        ensureWeb()

        // 两段式：先踢一脚，再轮询。
        //
        // **`evaluateJavascript` 不会等 Promise** —— 直接返回一个 async IIFE 的话，
        // 拿回来的是 Promise 对象的序列化，也就是 `{}`（踩过）。所以把结果写到
        // 一个全局变量上，再用第二个（同步的）注入去读。代价是两次往返，换来的是
        // 模型能写 `await fetch(...)` —— 这是这个动作最值钱的用法。
        val kick = """
            (function() {
              window.__agentJs = null;
              (async function() {
                try { window.__agentJs = {ok: await (async function() { $script })()}; }
                catch (e) { window.__agentJs = {err: String((e && e.message) || e)}; }
              })();
              return 'started';
            })()
        """.trimIndent()
        eval(kick)

        val limit = System.currentTimeMillis() + JS_TIMEOUT_MS
        while (System.currentTimeMillis() < limit) {
            val raw = eval(POLL_JS)
            val j = runCatching { JSONObject(raw) }.getOrNull()
            if (j != null && !j.isNull("pending")) {
                if (j.optInt("pending", 0) == 0) {
                    val e = j.optString("err")
                    if (e.isNotEmpty()) return errObj("JS 报错: $e")
                    var v = j.optString("ok")
                    if (v.length > TEXT_MAX_CHARS) v = v.take(TEXT_MAX_CHARS) + "\n…（已截断）"
                    return ok("JS 返回：\n$v")
                }
            }
            Thread.sleep(120)
        }
        return errObj("JS 超过 ${JS_TIMEOUT_MS / 1000}s 没返回")
    }

    /**
     * 截图：**软件绘制**（`View.draw`），不需要窗口、不需要屏幕亮着。
     *
     * 已知边界：Chromium 收到软件绘制会在 CPU 上重新栅格化，所以 DOM、普通
     * 图片、2D canvas 都在，但 **WebGL / `<video>` 这类只有 GPU 才有的层会是
     * 空白**（魔改版的解法是临时搭一个私有虚拟显示，代价是 300 行和一堆 OEM
     * 兼容问题 —— 先不做，等真的挡住事了再说）。
     */
    private fun screenshot(): JSONObject {
        val w = ensureWeb()
        val dm = app.resources.displayMetrics
        val pxW = w.width
        val pxH = w.height

        // 滚动的补偿：`View.draw(Canvas)` 不做父容器的 translate，
        // 直接画的话内容会往下偏一个 scrollY（顶部空白、底部被裁）。
        // 注意滚动位置在**合成器**里，不挂窗口时 `webView.scrollY` 恒为 0，
        // 所以只能问页面要。
        val scroll = runCatching {
            val a = JSONArray(eval("[Math.round(window.scrollX),Math.round(window.scrollY)]"))
            a.getInt(0) to a.getInt(1)
        }.getOrDefault(0 to 0)

        val bmp = onMain {
            val b = Bitmap.createBitmap(pxW, pxH, Bitmap.Config.ARGB_8888)
            val c = Canvas(b)
            c.translate(-(scroll.first * dm.density), -(scroll.second * dm.density))
            w.draw(c)
            b
        } as? Bitmap ?: return errObj("截图失败")

        val saved: File? = try {
            val dir = File(RootfsInstaller.rootfsDir(app), "root/.browser")
            if (!dir.exists() && !dir.mkdirs()) error("建不了目录 $dir")
            val f = File(dir, "shot-${System.currentTimeMillis()}.jpg")
            f.outputStream().use { bmp.compress(Bitmap.CompressFormat.JPEG, SHOT_QUALITY, it) }
            f
        } catch (t: Throwable) {
            Log.w(TAG, "截图落盘失败", t)
            null
        } finally {
            bmp.recycle()
        }
        if (saved == null) return errObj("截图存不下来（沙箱目录写不进？）")

        val guest = "$SHOT_GUEST_DIR/${saved.name}"
        val info = pageInfo()
        return ok(
            "已截图 ${saved.length() / 1024}KB（视口 ${pxW}×${pxH}px）",
            info?.optString("url").orEmpty(),
            info?.optString("title").orEmpty(),
        ).put("image", guest)
    }

    // -----------------------------------------------------------------------
    // JS 注入
    // -----------------------------------------------------------------------

    /**
     * 在主线程上执行一段 JS 并等结果。返回原始字符串（未加引号的）。
     *
     * 超时就认为这只 WebView 已经不可用了（[dead]）—— 渲染进程卡死时
     * `evaluateJavascript` 的回调**永远不会来**，再等下去就是白等。
     */
    private fun eval(js: String, timeout: Long = JS_TIMEOUT_MS): String {
        val latch = CountDownLatch(1)
        var out: String? = null
        val w = web ?: throw IllegalStateException("WebView 还没建")
        main.post {
            w.evaluateJavascript(js) { r ->
                // 解引号也可能抛（回调里抛出去没人接，闩就永远不动了）
                out = try {
                    unquote(r)
                } catch (t: Throwable) {
                    Log.w(TAG, "读 JS 结果失败", t)
                    "null"
                }
                latch.countDown()
            }
        }
        if (!latch.await(timeout, TimeUnit.MILLISECONDS)) {
            dead = true
            throw IllegalStateException("页面 ${timeout / 1000}s 没响应（渲染进程可能卡住了）")
        }
        return out ?: "null"
    }

    /**
     * `evaluateJavascript` 的回调把结果 **JSON 编码**过（字符串会带上引号和
     * 转义），用之前要解开。非字符串（数字/对象/null）原样返回。
     */
    private fun unquote(raw: String?): String {
        if (raw == null || raw == "null") return "null"
        if (raw.length < 2 || !raw.startsWith("\"") || !raw.endsWith("\"")) return raw
        return runCatching { JSONArray("[$raw]").getString(0) }.getOrDefault(raw)
    }

    /** eval 一层：结果是 JSON 就解出来，不是就当字符串塞进 `{text:…}` */
    private fun evalToJson(js: String): JSONObject? {
        val raw = eval(js)
        return runCatching { JSONObject(raw) }.getOrNull()
            ?: runCatching { JSONObject().put("text", raw) }.getOrNull()
    }

    // -----------------------------------------------------------------------
    // 通用：主线程往返、结果封装
    // -----------------------------------------------------------------------

    /** post 到主线程跑，等它做完（最多 [timeout]）。block **不能**再调 [onMain]/[eval] */
    private fun onMain(timeout: Long = 10_000L, block: () -> Any?): Any? {
        val latch = CountDownLatch(1)
        var out: Any? = null
        main.post {
            // ⚠️ `out = block()` 必须是**一条语句**，不能写成
            // `out = try { block() } finally { latch.countDown() }` ——
            // try/finally 是表达式：先求值、再跑 finally、**最后**才赋值，
            // 于是 countDown 发生在赋值之前，调用线程被叫醒时读到的还是 null。
            // 实测：模型那一轮报「建不出 WebView」，栈就在 ensureWeb，
            // 而且是 3ms 就返回（不是超时）——就是输在这个竞态上。
            try {
                out = block()
            } catch (t: Throwable) {
                Log.w(TAG, "主线程操作失败", t)
            } finally {
                latch.countDown()
            }
        }
        if (!latch.await(timeout, TimeUnit.MILLISECONDS)) {
            dead = true
            Log.w(TAG, "主线程 ${timeout}ms 没响应（标记 WebView 已坏）")
            throw IllegalStateException("WebView 主线程 ${timeout / 1000}s 没响应")
        }
        return out
    }

    private fun ok(text: String, url: String? = null, title: String? = null): JSONObject =
        JSONObject().apply {
            put("ok", true)
            put("text", text)
            if (!url.isNullOrEmpty()) put("url", url)
            if (!title.isNullOrEmpty()) put("title", title)
        }

    private fun errObj(msg: String): JSONObject =
        JSONObject().apply {
            put("ok", false)
            put("text", msg)
        }

    private fun err(msg: String): String = errObj(msg).toString()

    // -----------------------------------------------------------------------
    // JS 源码
    //
    // 每个动作是一段自包含的 IIFE，最后 `JSON.stringify(...)` 回结果。
    // 几个坑都写在代码里了（React 的受控输入、框架的变更通知、事件序列）。
    // -----------------------------------------------------------------------

    private fun jsQuote(s: String): String = buildString {
        for (ch in s) {
            when (ch) {
                '\\' -> append("\\\\")
                '\'' -> append("\\'")
                '"' -> append("\\\"")
                '\n' -> append("\\n")
                '\r' -> append("\\r")
                '\t' -> append("\\t")
                '\u2028' -> append("\\u2028")
                '\u2029' -> append("\\u2029")
                '$' -> append("\\$")
                else -> append(ch)
            }
        }
    }

    private fun clickSelJs(selector: String): String = """
        (function() {
          var el = document.querySelector('${jsQuote(selector)}');
          if (!el) return JSON.stringify({error: '没找到元素: ${jsQuote(selector)}'});
          if (el.disabled) return JSON.stringify({error: '元素是 disabled 的'});
          el.scrollIntoView({block: 'center'});
          ${CLICK_EVENTS}
          return JSON.stringify({clicked: true, tag: el.tagName, text: (el.innerText || el.value || '').substring(0, 100)});
        })()
    """.trimIndent()

    private fun clickXyJs(x: Int, y: Int): String = """
        (function() {
          var el = document.elementFromPoint($x, $y);
          if (!el) return JSON.stringify({error: '坐标 ($x, $y) 上没有元素'});
          ${CLICK_EVENTS}
          return JSON.stringify({clicked: true, tag: el.tagName, text: (el.innerText || el.value || '').substring(0, 100)});
        })()
    """.trimIndent()

    /**
     * 输入文本。
     *
     * 三件事必须都做，少一件就有站点点不动：
     * 1. **native setter** —— React/Vue 会在 `input.value` 上装自己的 setter
     *    记录"上一次的值"，直接赋值它看不见，于是框里有字、state 是空的。
     * 2. **逐字符 keydown/input/keyup** —— 靠监听按键的搜索框（防抖、逐字
     *    请求）只认事件。
     * 3. **框架同步** —— Angular 认 `$apply`，Vue 认 `$forceUpdate`。
     */
    private fun typeJs(selector: String, text: String): String = """
        (function() {
          var el = document.querySelector('${jsQuote(selector)}');
          if (!el) return JSON.stringify({error: '没找到元素: ${jsQuote(selector)}'});
          var val = '${jsQuote(text)}';
          el.focus();
          if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {
            var proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
            var setter = Object.getOwnPropertyDescriptor(proto, 'value');
            if (setter && setter.set) setter.set.call(el, val); else el.value = val;
          } else if (el.isContentEditable) {
            el.innerText = val;
          }
          for (var i = 0; i < val.length; i++) {
            var c = val[i];
            el.dispatchEvent(new KeyboardEvent('keydown', {key: c, bubbles: true}));
            el.dispatchEvent(new KeyboardEvent('keypress', {key: c, bubbles: true}));
            el.dispatchEvent(new InputEvent('input', {data: c, inputType: 'insertText', bubbles: true}));
            el.dispatchEvent(new KeyboardEvent('keyup', {key: c, bubbles: true}));
          }
          el.dispatchEvent(new Event('input', {bubbles: true}));
          el.dispatchEvent(new Event('change', {bubbles: true}));
          try {
            if (window.angular) {
              var ng = window.angular.element(el);
              var sc = (ng.scope && ng.scope()) || (ng.injector && ng.injector().get('${'$'}rootScope'));
              if (sc && !sc.${'$'}${'$'}phase) sc.${'$'}apply();
            }
          } catch (e) {}
          try { if (el.__vue__) el.__vue__.${'$'}forceUpdate(); } catch (e) {}
          // 丢掉焦点：很多表单/搜索框是 onblur 才提交或校验的
          el.dispatchEvent(new FocusEvent('blur', {bubbles: true, relatedTarget: null}));
          el.blur();
          return JSON.stringify({typed: true, length: val.length});
        })()
    """.trimIndent()

    private fun getTextJs(selector: String?): String {
        if (selector.isNullOrEmpty()) {
            return """
                (function() {
                  var t = (document.body.innerText || '').trim();
                  return JSON.stringify({text: t.substring(0, 40000), length: t.length, url: location.href, title: document.title});
                })()
            """.trimIndent()
        }
        return """
            (function() {
              var el = document.querySelector('${jsQuote(selector)}');
              if (!el) return JSON.stringify({error: '没找到元素: ${jsQuote(selector)}'});
              var t = (el.innerText || el.textContent || '').trim();
              return JSON.stringify({text: t.substring(0, 40000), length: t.length});
            })()
        """.trimIndent()
    }

    /**
     * 滚动。先试 window；页面没动说明滚动在某个内层容器里（现在满大街都是），
     * 就遍历 DOM 找"overflow 是 auto/scroll 且内容比容器高"里面**面积最大**的
     * 那个 —— 面积最大基本就是主内容区，不是侧边的小列表。
     */
    private fun scrollJs(direction: String, amount: Int, selector: String?): String {
        val px = if (direction == "up") -amount else amount
        val sel = if (selector.isNullOrEmpty()) "null" else "'${jsQuote(selector)}'"
        return """
            (function() {
              var sel = $sel;
              var px = $px;
              if (sel) {
                var el = document.querySelector(sel);
                if (!el) return JSON.stringify({error: '没找到元素: ' + sel});
                el.scrollBy(0, px);
                return JSON.stringify({scrolled: true, element: sel, scrollY: Math.round(el.scrollTop), scrollHeight: el.scrollHeight});
              }
              var before = window.scrollY;
              window.scrollBy(0, px);
              if (window.scrollY !== before || document.documentElement.scrollHeight <= window.innerHeight + 2) {
                return JSON.stringify({scrolled: true, element: 'window', scrollY: Math.round(window.scrollY), scrollHeight: document.documentElement.scrollHeight});
              }
              var best = null, bestArea = 0;
              var all = document.body.querySelectorAll('*');
              for (var i = 0; i < all.length; i++) {
                var e = all[i];
                var st = getComputedStyle(e);
                if ((st.overflowY === 'auto' || st.overflowY === 'scroll') && e.scrollHeight > e.clientHeight + 5) {
                  var area = e.clientWidth * e.clientHeight;
                  if (area > bestArea) { best = e; bestArea = area; }
                }
              }
              if (best) {
                best.scrollBy(0, px);
                return JSON.stringify({scrolled: true, element: best.tagName.toLowerCase(), scrollY: Math.round(best.scrollTop), scrollHeight: best.scrollHeight});
              }
              return JSON.stringify({scrolled: false, scrollY: Math.round(window.scrollY), scrollHeight: document.documentElement.scrollHeight});
            })()
        """.trimIndent()
    }

    /**
     * 页面骨架（模型“看”页面的主要方式）。
     *
     * 思路：不把 HTML 丢给模型（太大且全是噪音），而是抽一棵**可见元素树**：
     *
     * 1. 挑候选：可见、够大，而且是"有内容"的 —— 语义元素（a/button/input/img/…）
     *    或者自己直接带文字、且没有可见的元素子节点。
     * 2. **合并同位置的包装层**：如果某祖先的矩形和它几乎重合（±8px），
     *    代表提升到那个祖先。现代页面上百个无意义 div 靠这一步消失。
     * 3. 给每个节点算一个**唯一的短 selector**（有 id 用 id，否则从元素往上拼、
     *    拼到唯一为止），这是模型能"点"的前提。
     * 4. 内层和外层文字一样时只留内层（三层包装一份标题 = 三倍噪音）。
     * 5. 按面积建树、剪掉只有一个孩子的空壳、按 max_depth 截断。
     * 6. 直接在这里拼成缩进文本（模型读的就是它），并带上 rect。
     */
    private fun backboneJs(maxDepth: Int): String = """
        (function() {
          var MAX_DEPTH = $maxDepth;
          var MERGE = 8;
          var MIN = 6;
          var LIMIT = $BACKBONE_MAX_NODES;

          function vis(el) {
            var st = getComputedStyle(el);
            if (st.display === 'none' || st.visibility === 'hidden' || parseFloat(st.opacity) === 0) return false;
            var r = el.getBoundingClientRect();
            return r.width >= MIN && r.height >= MIN;
          }
          var SEM = {IMG:1, SVG:1, VIDEO:1, CANVAS:1, INPUT:1, TEXTAREA:1, SELECT:1, BUTTON:1, A:1, IFRAME:1, AUDIO:1};
          // 点它能出事儿的元素（合并时不许越过它们，见下面的 while）
          var CLICK = {A:1, BUTTON:1, INPUT:1, TEXTAREA:1, SELECT:1, SUMMARY:1};
          var SKIP = {SCRIPT:1, STYLE:1, NOSCRIPT:1, BR:1, HR:1, META:1, LINK:1, TEMPLATE:1, HEAD:1};
          function text(el) { return ((el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim()); }
          function isLeaf(el) {
            if (SKIP[el.tagName]) return false;
            if (SEM[el.tagName]) return true;
            var kids = el.children;
            for (var i = 0; i < kids.length; i++) {
              if (!SKIP[kids[i].tagName] && vis(kids[i])) return false;
            }
            return true;
          }
          function near(a, b) {
            return Math.abs(a.left - b.left) <= MERGE && Math.abs(a.top - b.top) <= MERGE &&
                   Math.abs(a.right - b.right) <= MERGE && Math.abs(a.bottom - b.bottom) <= MERGE;
          }
          // selector **必须唯一** —— 模型拿它去点，指到十几个元素上就点不动了。
          // 所以从元素往上拼，每加一段就问一次“现在唯一了吗”，唯一就停。
          // 代价是导航站那种“几百行一样结构”的表格会得到
          // `tr:nth-of-type(7) > td:nth-of-type(3) > a` 这种长 selector ——
          // 丑，但能点。（早先版本硬截成 3 段，于是每个链接的 selector
          // 全都一样，白忙一场。）
          function uniq(s) { try { return document.querySelectorAll(s).length === 1; } catch (e) { return false; } }
          // 数字开头的 id  **不能**写成 `#50051583`（CSS 里那是非法选择器，
          // querySelector 直接抛 SyntaxError）。导航站的行 id 全是这种，
          // 所以一律先验证，不行就用 [id="…"]。
          function idSel(id) {
            if (/^[A-Za-z_-][A-Za-z0-9_-]*$/.test(id)) return '#' + id;
            return '[id="' + String(id).replace(/"/g, '\\"') + '"]';
          }
          function sel(el) {
            if (el.id) { var byId = idSel(el.id); if (uniq(byId)) return byId; }
            var parts = [], cur = el, guard = 0;
            while (cur && cur !== document.body && cur !== document.documentElement && guard++ < 8) {
              if (cur.id) { parts.unshift(idSel(cur.id)); break; }
              var seg = cur.tagName.toLowerCase();
              var p = cur.parentElement;
              if (p) {
                var same = 0, idx = 0;
                for (var i = 0; i < p.children.length; i++) {
                  if (p.children[i].tagName === cur.tagName) { same++; if (p.children[i] === cur) idx = same; }
                }
                if (same > 1) seg += ':nth-of-type(' + idx + ')';
              }
              parts.unshift(seg);
              cur = p;
              if (parts.length >= 2 && uniq(parts.join(' > '))) return parts.join(' > ');
            }
            return parts.join(' > ');
          }

          var all = document.body.querySelectorAll('*');
          var nodes = [];
          for (var i = 0; i < all.length; i++) {
            var el = all[i];
            if (!vis(el) || !isLeaf(el)) continue;
            var r = el.getBoundingClientRect();
            var t = text(el);
            var interesting = t.length > 1 || SEM[el.tagName] ||
                              (el.tagName === 'A' && el.href);
            if (!interesting) continue;
            // 合并同位置的包装层：一路提升到最外层的"外壳"。
            // **碰见可点的语义元素就停**（a/button/input/…）—— 继续往上合并的话
            // selector 会指到一个 div/td/span 上，而点 wrapper 是点不出反应的
            // （我们是对元素本身派发事件，没有冒泡到内层 a 那回事）。
            var rep = el, rr = r;
            var up = el.parentElement;
            while (!CLICK[rep.tagName] && up && up !== document.body && up !== document.documentElement &&
                   vis(up) && near(rr, up.getBoundingClientRect())) {
              rep = up; rr = up.getBoundingClientRect();
              up = up.parentElement;
            }
            // 同一个代表只收一次
            var dup = false;
            for (var k = 0; k < nodes.length; k++) { if (nodes[k].el === rep) { dup = true; break; } }
            if (dup) continue;
            nodes.push({el: rep, rect: rr, area: rr.width * rr.height, text: t.substring(0, 200)});
          }

          // 外层和内层文字一模一样时只留内层。
          // 现代页面上一个标题常常被包三层（td > span.titleline > a），
          // 三份一样的文字就是三倍的噪音；而且**能点的通常是内层那个 a**。
          var kept = [];
          for (var i = 0; i < nodes.length; i++) {
            var n = nodes[i], dupText = false;
            if (n.text) {
              for (var j = 0; j < nodes.length; j++) {
                if (j !== i && nodes[j].el !== n.el && n.el.contains(nodes[j].el) && nodes[j].text === n.text) { dupText = true; break; }
              }
            }
            if (!dupText) kept.push(n);
          }
          nodes = kept;

          nodes.sort(function(a, b) { return b.area - a.area; });
          function contains(a, b) { return a.el !== b.el && a.el.contains(b.el); }
          var roots = [];
          for (var i = 0; i < nodes.length; i++) {
            var n = nodes[i], parent = null, best = Infinity;
            for (var j = 0; j < i; j++) {
              if (contains(nodes[j], n) && nodes[j].area < best) { parent = nodes[j]; best = nodes[j].area; }
            }
            if (parent) { (parent.kids = parent.kids || []).push(n); } else { roots.push(n); }
          }
          // 只有一个孩子的空壳：把孩子的身份提上来
          function prune(n) {
            (n.kids || []).forEach(prune);
            if ((!n.kids || n.kids.length === 1) && !n.text) {
              var c = n.kids && n.kids[0];
              if (c) { n.el = c.el; n.rect = c.rect; n.text = c.text; n.kids = c.kids; }
            }
            if (n.kids) n.kids = n.kids.filter(function(k) { return (k.kids && k.kids.length) || k.text || SEM[k.el.tagName]; });
          }
          function textOf(n) { return (n.text || '').substring(0, 120); }
          roots.forEach(prune);
          roots = roots.filter(function(r) { return textOf(r) || SEM[r.el.tagName] || (r.kids && r.kids.length); });

          var lines = [], count = 0, depth = 0;
          function emit(n, d, pad) {
            if (count >= LIMIT) return;
            count++; if (d > depth) depth = d;
            var el = n.el, r = n.rect;
            var head = pad + '<' + el.tagName.toLowerCase() + '>';
            if (el.id) head += ' #' + el.id;
            var cls = (typeof el.className === 'string' ? el.className : '').trim().split(/\s+/).filter(Boolean).slice(0, 2);
            if (cls.length) head += ' .' + cls.join('.');
            head += ' | ' + sel(el);
            head += ' | ' + Math.round(r.left) + ',' + Math.round(r.top) + ' ' + Math.round(r.width) + 'x' + Math.round(r.height);
            lines.push(head);
            var t = textOf(n);
            if (t) lines.push(pad + '  "' + t + '"');
            if (el.tagName === 'A' && el.href) {
              try {
                var u = new URL(el.href);
                // 跨域链接要带上域名，否则模型只看到 /article/xxx 以为还在本站
                var host = (u.host === location.host) ? '' : (u.host + ' ');
                lines.push(pad + '  -> ' + host + u.pathname + (u.search || ''));
              }
              catch (e) { lines.push(pad + '  -> ' + el.href.substring(0, 100)); }
            }
            if (el.tagName === 'IMG') lines.push(pad + '  img ' + (el.naturalWidth || 0) + 'x' + (el.naturalHeight || 0) + ' ' + (el.currentSrc || el.src || '').substring(0, 100));
            if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT') {
              var tp = el.type || el.tagName.toLowerCase();
              var v = el.type === 'password' ? '[redacted]' : (el.value || '');
              lines.push(pad + '  input ' + tp + (v ? ' value="' + String(v).substring(0, 60) + '"' : '') + (el.placeholder ? ' placeholder="' + el.placeholder.substring(0, 60) + '"' : ''));
            }
            if (d >= MAX_DEPTH) return;
            (n.kids || []).forEach(function(k) { emit(k, d + 1, pad + '  '); });
          }
          roots.forEach(function(r) { emit(r, 1, ''); });
          if (!lines.length) lines.push('（这一屏没有可读的可见元素）');
          return JSON.stringify({text: lines.join('\n'), nodes: count, depth: depth});
        })()
    """.trimIndent()

    /**
     * 页面里的 `target=_blank` / `window.open` 不另开窗口（我们的 WebView 没有
     * 多窗口能力，点了会**什么都不发生** —— 模型只会看到"点了但没反应"）。
     * 每页开始加载时把 `_blank` 掰成 `_self`，链接就在原地打开。
     */
    private fun targetFixJs(): String = """
        (function() {
          if (window.__agentTargetFix) return;
          window.__agentTargetFix = 1;
          document.addEventListener('click', function(e) {
            var a = e.target && e.target.closest && e.target.closest('a[target]');
            if (a && a.target && a.target !== '_self') a.target = '_self';
          }, true);
        })()
    """.trimIndent()

    private val CLICK_EVENTS = """
          var mk = function(type, bubbles) {
            return new MouseEvent(type, {bubbles: bubbles, cancelable: true, view: window});
          };
          el.dispatchEvent(mk('mouseover', true));
          el.dispatchEvent(mk('mouseenter', false));
          el.dispatchEvent(mk('mousemove', true));
          el.dispatchEvent(mk('mousedown', true));
          el.dispatchEvent(mk('mouseup', true));
          el.click();
          el.dispatchEvent(mk('mouseleave', false));
          el.dispatchEvent(mk('mouseout', true));
    """.trimIndent()

    private val TARGET_FIX_JS = targetFixJs()

    /** 读 [runJs] 踢出去的那一脚的结果。`pending:1` = 还没结束 */
    private val POLL_JS = """
        (function() {
          var v = window.__agentJs;
          if (v === null || v === undefined) return JSON.stringify({pending: 1});
          if (v.err) return JSON.stringify({pending: 0, err: String(v.err)});
          return JSON.stringify({pending: 0, ok: (typeof v.ok === 'string' ? v.ok : JSON.stringify(v.ok))});
        })()
    """.trimIndent()
}