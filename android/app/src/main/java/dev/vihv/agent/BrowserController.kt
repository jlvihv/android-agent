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

        /** 页面停止后至少再等这么一会儿：JS 渲染（SPA）大多在这一小段里完成 */
        private const val SETTLE_MS = 300L

        /** 动作后的“等页面安定”预算：先用短的，不够再显式调 wait_for_dom_stable */
        private const val SETTLE_BUDGET_MS = 4_000L

        /** 骨架树给模型的文本上限（它会进历史，按钱算） */
        private const val BACKBONE_MAX_CHARS = 12_000
        private const val BACKBONE_MAX_NODES = 220

        /** 正文上限 */
        private const val TEXT_MAX_CHARS = 12_000

        /** 截图目录（沙箱里的路径，`read` 那个工具能直接读） */
        private const val SHOT_GUEST_DIR = "/root/.browser"

        /** 截图压缩质量。截图进的是模型上下文，不是相册 */
        private const val SHOT_QUALITY = 80

        /** 整页截图的高度上限（物理像素）。1080×8000 的 ARGB 位图 ≈ 34MB */
        private const val FULL_PAGE_MAX_PX = 8_000

        /**
         * 标签页上限。每只 WebView 都要占一块渲染内存，而且模型同时盯三页就已经
         * 转不过来了；OpenMinis 那边也是 3 起步。满了就让模型先关一只。
         */
        private const val MAX_TABS = 4

        /**
         * 桌面版的 UA 和视口。
         *
         * **两个必须一起换**：只换 UA 不换视口的话，桌面版布局塞进 411px 宽，
         * 排版比移动版还乱。视口是 CSS px，1180×820 接近一台笔记本。
         */
        private const val DESKTOP_UA =
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/134.0.0.0 Safari/537.36"
        private val DESKTOP_VIEWPORT = 1180 to 820

        /** 拿不到系统 UA 时的兑底（正常应总是能拿到）*/
        private const val DEFAULT_MOBILE_UA =
            "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/134.0.0.0 Mobile Safari/537.36"
    }

    /** 串行化动作。见类注释 */
    private val lock = ReentrantLock()

    private val main = Handler(Looper.getMainLooper())

    /** 手机自己的 UA（建 WebView 时记下来，切桌面版后能切回）*/
    @Volatile
    private var defaultUa: String = DEFAULT_MOBILE_UA

    /**
     * 一个标签页：一只 WebView + 它自己的加载进度 / 错误 / 死活。
     *
     * 为什么这些都挂在 tab 上而不是控制器上：多个标签页会**同时**在加载。
     * 计数器和错误如果还是全局的，一个后台标签的加载就会把前台的等待骗过去
     * （`awaitLoad` 看到计数变了就以为自己的页好了）。
     */
    private class Tab(val id: Int, val web: WebView) {
        /** 页每次开始加载 +1（`onPageStarted`）*/
        val started = AtomicInteger()

        /** 页每次加载结束 +1（`onPageFinished` / 主文档出错 / 渲染进程死）*/
        val finished = AtomicInteger()

        /**
         * 渲染进程死了（`onRenderProcessGone`）或 JS 一直不回调。
         * 已死的 WebView 调什么都不会有反应，只能整只换掉（[ensureWeb]）。
         */
        @Volatile
        var dead = false

        /** 主文档的加载错误，navigate 读了报给模型 */
        @Volatile
        var lastError: String? = null
    }

    /** 主线程独占：所有标签页（顺序就是面板上从左到右）。 */
    private val tabs = mutableListOf<Tab>()

    /** 主线程独占：当前标签在 [tabs] 里的下标 */
    private var currentIdx = 0

    /** 标签 id 只增不重用 —— 模型提到"标签 2"时不会指向另一只页 */
    private var nextTabId = 1

    /**
     * 当前动作操作的那只 WebView。
     *
     * 保留这个字段（而不是每处都写 `curTab()?.web`）是为了让下面几十个动作方法
     * 不用改；它只在主线程换，其它线程只读。
     */
    @Volatile
    private var web: WebView? = null

    /** 当前动作的目标标签。一个动作从开始到结束都认它 */
    @Volatile
    private var active: Tab? = null

    /** 主线程：当前标签 */
    private fun curTab(): Tab? = tabs.getOrNull(currentIdx)

    /**
     * 断言自己在主线程。
     *
     * 为什么值得写：面板那几个入口（[pageState] / [selectTabFromUi] / [attachTo]）
     * 是**从主线程**调的，而动作那几个入口是从 Rust 的 spawn_blocking 线程调的、
     * 内部用 [onMain] 跳过去。两种姿势写混的后果很重：主线程里调 [onMain] 会
     * “自己等自己” 10 秒，然后抛异常 → **点一下标签条就闪退**（实测）。
     * 有这行断言，就变成一眼能看懂的报错。
     */
    private fun onMainThread(): Boolean {
        check(Looper.myLooper() == Looper.getMainLooper()) {
            "这个方法只能在主线程调（面板入口）；从别的线程来要用 onMain { }"
        }
        return true
    }

    /** 主线程：换当前标签，并把 [web] / [active] 同步过去 */
    private fun setCurrent(i: Int) {
        if (tabs.isEmpty()) {
            currentIdx = 0
            web = null
            active = null
            onPagesChanged?.invoke()
            return
        }
        currentIdx = i.coerceIn(0, tabs.size - 1)
        web = tabs[currentIdx].web
        active = tabs[currentIdx]
        onPagesChanged?.invoke()
    }

    /**
     * 正在跑一个动作。给看板面板显示“agent 正在操作…”用。
     *
     * **不用它去堵用户的手**：用户和 agent 看的是同一个页面，本来就会互相影响
     * （OpenMinis 是在忙的时候禁掉面板上的控件，但我们这层只是“看”，先不再加锁）。
     */
    @Volatile
    var busy: Boolean = false
        private set

    // 页面加载的进度计数。为什么不用 CountDownLatch：一次动作（点击）**可能**
    // 触发导航，也可能不触发，而导航还可能重定向好几次（多次 start/finish）。
    // 计数器比一次性闩好描述这类事。判据“一次加载跑完了” =
    // `started > 快照 && finished >= started`。
    private fun navCounters(): Pair<Int, Int> {
        val t = active ?: return 0 to 0
        return t.started.get() to t.finished.get()
    }

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
     * 等页面安定：DOM 的“指纹”连续 3 次读数一致才算稳。
     *
     * **为什么在 App 侧轮询，而不是在页面里 `setTimeout` 自己退**：页面在不挂窗口
     * 的 WebView 里（大概率还切了后台）会被 Chromium 把定时器节流到秒级 ——
     * 写在页面里的轮询会又慢又不准。App 侧注入是主动执行的，不受这个影响。
     *
     * 指纹用「元素个数 + 可见文字长度 + 滚动高度」：三个都能被内容变化推动，
     * 而计算它们不需要序列化整个 DOM（重页面 serialize 一次就卡住了）。
     */
    private fun awaitDomStable(timeoutMs: Long) {
        val deadline = System.currentTimeMillis() + timeoutMs
        var last = ""
        var same = 0
        while (System.currentTimeMillis() < deadline) {
            val sig = runCatching { eval(DOM_SIG_JS) }.getOrNull()
            if (sig == null) return // 页面可能正在换文档（导航），当作“还在动”
            if (sig == last && sig != "null") {
                same++
                if (same >= 3) return
            } else {
                same = 0
            }
            last = sig
            Thread.sleep(250)
        }
    }

    /** 动作之后的统一“安定”流程：先看它跳没跳，再等 DOM 稳。*/
    private fun settleAfterAction(started0: Int) {
        val navigating = waitForNavStart(started0, 1200)
        if (navigating) awaitLoad(started0, 20_000)
        else Thread.sleep(SETTLE_MS)
        awaitDomStable(SETTLE_BUDGET_MS)
    }

    /** 等一次新的页面加载开始。`true` = 真的开始加载了 */
    private fun waitForNavStart(started0: Int, maxMs: Long): Boolean {
        val deadline = System.currentTimeMillis() + maxMs
        while (System.currentTimeMillis() < deadline) {
            if (navCounters().first > started0) return true
            Thread.sleep(60)
        }
        return false
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
            busy = true
            return try {
                dispatch(action)
            } catch (t: Throwable) {
                Log.w(TAG, "动作失败: $actionJson", t)
                err(t.message ?: t.toString())
            } finally {
                busy = false
            }
        }
    }

    // -----------------------------------------------------------------------
    // 给看板面板（MainActivity）用的几个接口。都在主线程调。
    // -----------------------------------------------------------------------

    /**
     * `hasPage()` 可能变了就回调一次（开第一只标签 / 把标签关光）。
     * **主线程**调。
     *
     * MainActivity 的头部靠它决定显不显示「看浏览器」那个地球按钮：
     * 没开过页面时按钮点了只会弹一句「还没打开过网页」，那就不如不显示。
     * `setCurrent` 是标签集合和 [web] 唯一的改动点，挂在那里就覆盖全了。
     */
    var onPagesChanged: (() -> Unit)? = null

    /** agent 到底有没有开过页面（没开过就不要打开看板）。 */
    fun hasPage(): Boolean = web != null

    /**
     * 给面板的头用的状态：URL / 标题 / 忙不忙 / 各标签的短名 + 当前是第几个。
     * **只在主线程调**。
     */
    fun pageState(): PanelState {
        onMainThread()
        val labels = tabs.map { t ->
            val u = runCatching { t.web.url }.getOrNull().orEmpty()
            val host = u.substringAfter("://", "").substringBefore('/').removePrefix("www.")
            when {
                host.isEmpty() -> "新标签"
                // IP（含端口）不截：`127.0.0.1:3392` 截一下就看不出是哪儿了
                host[0].isDigit() -> host
                // 域名只留最后两段：en.wikipedia.org → wikipedia.org
                else -> host.split('.').takeLast(2).joinToString(".")
            }
        }
        return PanelState(
            url = web?.url ?: "",
            title = web?.title ?: "",
            busy = busy,
            currentIdx = currentIdx,
            labels = labels,
        )
    }

    /**
     * 面板上点了某个标签。**必须主线程调**（点它的地方就是主线程）。
     *
     * agent 正在跑动作时忽略这次点击（tryLock 拿不到就返回）—— 那时的页面
     * 是它在操作，半路把当前标签换掉会让它的下一步落在错的页上。
     */
    fun selectTabFromUi(idx: Int) {
        onMainThread()
        if (!lock.tryLock()) return
        try {
            setCurrent(idx)
            refreshAttached()
        } finally {
            lock.unlock()
        }
    }

    /** 面板正挂着的容器（没有就是 null）。**主线程独占**。 */
    private var panelHolder: android.view.ViewGroup? = null

    /** 切/关标签之后，把面板里的那只换成新的当前标签。**主线程调**。 */
    private fun refreshAttached() {
        panelHolder?.let { attachTo(it) }
    }

    data class PanelState(
        val url: String,
        val title: String,
        val busy: Boolean,
        val currentIdx: Int,
        /** 每个标签一个短名（域名），顺序就是面板上从左到右 */
        val labels: List<String>,
    )

    /**
     * 把这只 WebView 临时挂进一个容器，让用户看 / 接管。**主线程调**。
     *
     * 这就是“无头”和“可见”之间唯一的关系：同一只 WebView，需要给人看的时候
     * 才临时挂上窗口，关掉面板就摘下来 —— agent 照跑（页面不重载、状态不丢）。
     * 绝不另建一只，否则用户看到的和 agent 操作的不是同一个页面。
     */
    fun attachTo(container: android.view.ViewGroup) {
        onMainThread()
        panelHolder = container
        val w = web ?: return
        (w.parent as? android.view.ViewGroup)?.removeView(w)
        container.removeAllViews()
        container.addView(
            w,
            android.view.ViewGroup.LayoutParams(
                android.view.ViewGroup.LayoutParams.MATCH_PARENT,
                android.view.ViewGroup.LayoutParams.MATCH_PARENT,
            ),
        )
        Log.i(TAG, "面板已挂上（当前 ${w.url}）")
    }

    /** 从面板上摘下来，还它一个无头身。**主线程调**。 */
    fun detach() {
        onMainThread()
        panelHolder = null
        val w = web ?: return
        (w.parent as? android.view.ViewGroup)?.removeView(w)
        Log.i(TAG, "面板已摘下（agent 继续跑在无头模式）")
    }

    private fun dispatch(a: JSONObject): String {
        val name = a.optString("action")
        Log.i(TAG, "action=$name ${a.toString().take(200)}")

        // 标签管理那几个动作**不要**顺手建标签：模型问“有哪些标签”不该因此多出
        // 一只空标签（踩过：list_tabs 之前先跑 ensureWeb，结果每次列完都多一只）。
        val managesTabs = name == "new_tab" || name == "list_tabs" ||
            name == "close_tab" || name == "switch_tab"

        // 给了 `tab` 就先**切过去**再操作（和点浏览器的标签一个语义）。
        // 这样“当前标签”永远就是动作实际发生的那一只，模型不用在脑子里记账。
        if (a.has("tab") && !managesTabs) {
            val want = a.optInt("tab")
            val ok = onMain {
                val at = tabs.indexOfFirst { it.id == want }
                if (at >= 0) { setCurrent(at); true } else false
            } as? Boolean ?: false
            if (!ok) {
                return err("没有标签 ${want}（当前有：${onMain { tabs.map { it.id } } ?: emptyList<Int>()}）")
            }
        } else if (!managesTabs) {
            // 没给 tab：确保当前标签存在（第一次用浏览器时这里建第一只）
            runCatching { ensureWeb() }
        }

        curTab()?.lastError = null

        val out: JSONObject = when (name) {
            "navigate" -> navigate(a.optString("url"))
            "get_backbone" -> getBackbone(a.optInt("max_depth", 5))
            "click" -> click(a)
            "type" -> type(a.optString("selector"), a.optString("text"))
            "get_text" -> getText(a.optString("selector").ifEmpty { null })
            "get_readable" -> getReadable()
            "wait_for_dom_stable" -> waitForDomStable(a)
            "scroll" -> scroll(a)
            "screenshot" -> screenshot(a.optBoolean("full_page", false))
            "back" -> goBack()
            "press" -> press(a.optString("key"))
            "find_elements" -> findElements(a.optString("selector"))
            "set_user_agent" -> setUserAgent(a.optString("user_agent"))
            "js" -> runJs(a.optString("script"))
            "new_tab" -> newTab(a.optString("url").ifEmpty { null })
            "list_tabs" -> listTabs()
            "switch_tab" -> switchTab(a.optInt("tab", -1))
            "close_tab" -> closeTab(a.optInt("tab", 0))
            else -> return err("未知动作: $name")
        }

        // 每个动作回都带上"我在哪只标签上"：模型多标签时靠它记账
        out.put("tab", onMain { curTab()?.id } ?: 0)
        out.put("tabs", onMain { tabs.size } ?: 0)

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
            var t = curTab()
            if (t == null) {
                // 一个标签都没有（刚启动，或者刚把所有标签关光）
                t = createTab(null)
                tabs.add(t)
                setCurrent(tabs.size - 1)
            } else if (t.dead) {
                Log.w(TAG, "标签 ${t.id} 坏了，就地重开一只")
                val at = currentIdx
                val keep = runCatching { t.web.url }.getOrNull()
                runCatching { t.web.destroy() }
                val fresh = createTab(keep)
                tabs[at] = fresh
                setCurrent(at)
                t = fresh
            }
            active = t
            t.web
        }
        if (got is WebView) return got
        throw IllegalStateException("WebView 建不出来（上一行日志里有原因）")
    }

    // -----------------------------------------------------------------------
    // 标签页
    // -----------------------------------------------------------------------

    /** 建新标签（给模型和面板用）。**主线程调** */
    private fun newTabAt(url: String?): Tab {
        val t = createTab(url)
        tabs.add(t)
        setCurrent(tabs.size - 1)
        return t
    }

    /** 关一只标签（给模型和面板用）。**主线程调** */
    private fun dropTab(tab: Tab) {
        val at = tabs.indexOf(tab)
        if (at < 0) return
        tabs.removeAt(at)
        // 面板正挂着它的话先摘下来，再销毁
        (tab.web.parent as? android.view.ViewGroup)?.removeView(tab.web)
        runCatching { tab.web.destroy() }
        if (currentIdx >= at) setCurrent(currentIdx - 1) else setCurrent(currentIdx)
        Log.i(TAG, "关掉标签 ${tab.id}（剩 ${tabs.size} 只）")
    }

    /**
     * 建一只标签页（新 WebView + 把回调挂到**它自己的**计数器上）。
     *
     * 先建 WebView、再建 Tab、再 `configure`，是因为回调要引用 tab 的计数器；
     * 反过来（先 tab 后 view）在 Kotlin 里就得用可空变量兜圈子。
     */
    private fun createTab(url: String?): Tab {
        val v = WebView(app)
        val tab = Tab(nextTabId++, v)
        configureWebView(v, tab)
        if (!url.isNullOrEmpty()) v.loadUrl(url)
        return tab
    }

    private fun configureWebView(v: WebView, tab: Tab) {
        // 记住手机自己的 UA：切到桌面版之后还能切回来
        defaultUa = v.settings.userAgentString ?: DEFAULT_MOBILE_UA
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
                tab.finished.incrementAndGet()
            }

            override fun onReceivedError(
                view: WebView?,
                request: WebResourceRequest?,
                error: android.webkit.WebResourceError?,
            ) {
                // 只记主文档：子资源失败（一张图 404）不该让整个 navigate 变红
                if (request?.isForMainFrame == true) {
                    tab.lastError = "加载失败: ${error?.description}（${error?.errorCode}）"
                    tab.finished.incrementAndGet()
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
                Log.w(TAG, "标签 ${tab.id} 的渲染进程没了（crashed=${detail?.didCrash()}），下次换一只")
                tab.dead = true
                tab.finished.incrementAndGet()
                return true
            }

            /** 每页开始加载时把 target=_blank 掰成 _self（见 installTargetFix） */
            override fun onPageStarted(view: WebView?, url: String?, favicon: android.graphics.Bitmap?) {
                tab.started.incrementAndGet()
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
        applyViewport(v, null)
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

    /**
     * 归一化 URL：没写协议头按 https 补；不是 http(s) 就返回 null。
     * 挡的是 `file://`（读本地文件）、`javascript:`、`intent://`（拉起别的 App）这些。
     */
    private fun normalizeUrl(raw: String): String? {
        val url = raw.trim()
        if (url.isEmpty()) return null
        val target = if (url.contains("://")) url else "https://$url"
        val scheme = runCatching { android.net.Uri.parse(target).scheme?.lowercase() }.getOrNull()
        return if (scheme == "http" || scheme == "https") target else null
    }

    private fun navigate(urlRaw: String?): JSONObject {
        val url = (urlRaw ?: "").trim()
        if (url.isEmpty()) return errObj("缺少参数 url")
        val target = normalizeUrl(url) ?: return errObj("只支持 http/https：$url")

        val w = ensureWeb()
        val snap = navCounters().first
        onMain { w.loadUrl(target) }
        val finished = awaitLoad(snap, NAV_TIMEOUT_MS)
        if (!finished) {
            // SPA 常常不触发 onPageFinished；不是失败，把能读到的如实报出来
            Log.i(TAG, "等 onPageFinished 超时，照样往下读")
        }
        // JS 渲染出来的内容大多在这一小段里出现
        awaitDomStable(SETTLE_BUDGET_MS)

        val info = pageInfo()
        val title = info?.optString("title").orEmpty()
        val realUrl = info?.optString("url").orEmpty().ifEmpty { target }
        val text = buildString {
            append("已打开 ")
            append(realUrl)
            if (title.isNotEmpty()) append("\n标题：$title")
            if (!finished) append("\n（页面可能还在加载）")
            active?.lastError?.let { append("\n$it") }
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
                    innerHeight: window.innerHeight,
                    elements: document.querySelectorAll('*').length
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
        val matched = out.optInt("matched", 1)
        val extra = if (matched > 1) "（selector 匹配到 $matched 个，点的是第一个）" else ""
        return ok(
            "已点击 <${out.optString("tag")}> ${out.optString("text").take(80)}$extra",
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
        val matched = out.optInt("matched", 1)
        val extra = if (matched > 1) "（selector 匹配到 $matched 个，填的是第一个）" else ""
        return ok("已往 ${sel} 输入 ${out.optInt("length")} 个字符$extra")
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

    /**
     * 抽正文（文章页用它，不要用 get_text）。
     *
     * `get_text` 给的是整个 body 的 innerText，导航/页脚/推荐位/广告语全在上面 ——
     * 实测一篇 CNBC 报道拿回 6415 字，一半是菜单和免责声明。这里先找“最像正文的
     * 容器”（段落密度 + 链接密度惩罚），再按文档顺序只取标题/段落/列表项/引用/代码，
     * 顺带去重、去掉导航块。
     */
    private fun getReadable(): JSONObject {
        ensureWeb()
        val out = evalToJson(READABLE_JS) ?: return errObj("抽正文失败")
        out.optString("error").takeIf { it.isNotEmpty() }?.let { return errObj(it) }

        val title = out.optString("title")
        val desc = out.optString("description")
        var text = out.optString("text")
        val chars = out.optInt("length")
        if (text.length > TEXT_MAX_CHARS) {
            text = text.take(TEXT_MAX_CHARS) + "\n…（已截断）"
        }
        val head = buildString {
            append("正文")
            if (title.isNotEmpty()) append("：【$title】")
            append("（原文 $chars 字，来源 ${out.optString("source")}")
            append(if (desc.isNotEmpty()) "；描述：${desc.take(160)}" else "")
            append("）")
        }
        return ok("$head\n$text")
    }

    // -----------------------------------------------------------------------
    // 标签页动作
    // -----------------------------------------------------------------------

    /** 开一只新标签（可选直接打开 url） */
    private fun newTab(url: String?): JSONObject {
        val have = onMain { tabs.size } as? Int ?: 0
        if (have >= MAX_TABS) {
            return errObj(
                "标签太多（上限 $MAX_TABS）。先 close_tab 关掉不用的，或者 list_tabs 看看现在有哪些。",
            )
        }
        val target = if (url.isNullOrEmpty()) null else normalizeUrl(url)
        if (!url.isNullOrEmpty() && target == null) return errObj("只支持 http/https：$url")

        val t = onMain { newTabAt(null) } as? Tab ?: return errObj("开标签失败")
        if (target != null) {
            // 先建空标签切过去，再自己 loadUrl：这样计数器的快照一定是这只页的
            val snap = navCounters().first
            onMain { t.web.loadUrl(target) }
            awaitLoad(snap, NAV_TIMEOUT_MS)
            awaitDomStable(SETTLE_BUDGET_MS)
        }
        onMain { refreshAttached() }
        val info = pageInfo()
        val what = if (target != null) "已打开 $target" else "空白页（接着用 navigate）"
        return ok(
            "开了标签 ${t.id}（共 ${onMain { tabs.size }} 只）：$what",
            info?.optString("url").orEmpty(),
            info?.optString("title").orEmpty(),
        )
    }

    /** 列出所有标签。多标签时模型靠它记账 */
    private fun listTabs(): JSONObject {
        val out = onMain {
            val cur = curTab()?.id
            if (tabs.isEmpty()) return@onMain "（一个标签都没有）"
            tabs.joinToString("\n") { t ->
                val u = runCatching { t.web.url }.getOrNull().orEmpty()
                val ti = runCatching { t.web.title }.getOrNull().orEmpty()
                val mark = if (t.id == cur) "→" else " "
                val title = if (ti.isBlank()) "(空)" else ti.take(60)
                "$mark 标签 ${t.id}：$title — ${u.take(110)}"
            }
        } as? String ?: ""
        val cur = onMain { curTab()?.id } ?: 0
        return ok("共 ${onMain { tabs.size }} 只（当前是标签 $cur）：\n$out")
    }

    /** 切到某只标签 */
    private fun switchTab(id: Int): JSONObject {
        val okSwitch = onMain {
            val at = tabs.indexOfFirst { it.id == id }
            if (at < 0) return@onMain false
            setCurrent(at)
            refreshAttached()
            true
        } as? Boolean ?: false
        if (!okSwitch) {
            val have = onMain { tabs.map { it.id } } ?: emptyList<Int>()
            return errObj("没有标签 $id（现有：$have；用 list_tabs 看）")
        }
        val info = pageInfo()
        return ok(
            "切到标签 $id",
            info?.optString("url").orEmpty(),
            info?.optString("title").orEmpty(),
        )
    }

    /** 关一只标签。不给 `tab` 就是关当前这只 */
    private fun closeTab(id: Int): JSONObject {
        val target = if (id > 0) id else (onMain { curTab()?.id } as? Int ?: 0)
        if (target == 0) return errObj("没有标签可关（用 list_tabs 看）")
        val okClose = onMain {
            val t = tabs.firstOrNull { it.id == target } ?: return@onMain false
            dropTab(t)
            refreshAttached()
            true
        } as? Boolean ?: false
        if (!okClose) return errObj("没有标签 $target")
        val left = onMain { tabs.size } as? Int ?: 0
        if (left == 0) return ok("关掉标签 $target，现在一个标签都不剩了（下一次动作会开一只新的）")
        val info = pageInfo()
        return ok(
            "关掉标签 $target，剩 $left 只（当前是标签 ${onMain { curTab()?.id } ?: 0}）",
            info?.optString("url").orEmpty(),
            info?.optString("title").orEmpty(),
        )
    }

    /** 等 DOM 安定（动态内容/无限滚动靠它）。`timeout` 秒，默认 10 */
    /**
     * 按一个键。最常见的是搜索框填完要回车。
     *
     * 一个必须自己补的事：**合成事件不会触发表单的隐式提交**。
     * 真浏览器里“在输入框里敲回车 → 提交表单”是 trusted event 的行为，
     * `dispatchEvent` 出来的 keydown 不会引发它。所以 Enter 要显式
     * `form.requestSubmit()`（没有就点 submit 按钮）—— 两件事都做：
     * 很多现代站是自己在 keydown 里听 Enter，两条路各盖一半。
     *
     * Tab 同理：合成事件不移动焦点，得自己算可聚焦元素列表。
     */
    private fun press(key: String?): JSONObject {
        val k = (key ?: "").trim()
        if (k.isEmpty()) return errObj("缺少参数 key")
        ensureWeb()
        val snap = navCounters().first
        val out = evalToJson(pressJs(k)) ?: return errObj("按键失败")
        out.optString("error").takeIf { it.isNotEmpty() }?.let { return errObj(it) }
        settleAfterAction(snap)
        val acted = out.optString("acted")
        val tail = if (acted.isEmpty()) "" else "（$acted）"
        return ok("已按 $k，焦点在 <${out.optString("tag")}>$tail")
    }

    /** 列出 selector 匹配到的元素（多少个、各自的文字/坐标） */
    private fun findElements(selector: String?): JSONObject {
        if (selector.isNullOrEmpty()) return errObj("缺少参数 selector")
        ensureWeb()
        val out = evalToJson(findElementsJs(selector)) ?: return errObj("查找失败")
        out.optString("error").takeIf { it.isNotEmpty() }?.let { return errObj(it) }

        val count = out.optInt("count")
        val shown = out.optInt("shown")
        val items = out.optJSONArray("items")
        val sb = StringBuilder()
        sb.append("$selector 匹配 $count 个")
        if (shown < count) sb.append("，列前 $shown 个")
        sb.append("（x,y 可直接给 click；看不见的先 scroll）\n")
        if (items != null) {
            for (i in 0 until items.length()) {
                val it = items.optJSONObject(i) ?: continue
                val vis = if (it.optBoolean("visible")) "可见" else "不可见"
                sb.append("${it.optInt("i")}. <${it.optString("tag")}> ${it.optString("text").take(60)}")
                sb.append(" | x,y=${it.optInt("x")},${it.optInt("y")} ${it.optInt("w")}x${it.optInt("h")} $vis\n")
                val href = it.optString("href")
                if (href.isNotEmpty()) sb.append("   -> ${href.take(140)}\n")
            }
        }
        return ok(sb.toString().trimEnd())
    }

    /**
     * 切 UA（`mobile` / `desktop`）。连着**视口一起切** —— 只换 UA 不换视口
     * 反而更糟：桌面版布局塞进 411px 宽，排版比移动版还乱。
     *
     * 换完要重载页面（UA 是请求头，不重载不生效），然后等加载。
     */
    private fun setUserAgent(profile: String?): JSONObject {
        val want = (profile ?: "").trim().lowercase()
        if (want != "mobile" && want != "desktop") {
            return errObj("user_agent 只能是 mobile 或 desktop")
        }
        val w = ensureWeb()
        val snap = navCounters().first
        onMain {
            w.settings.userAgentString = if (want == "desktop") DESKTOP_UA else defaultUa
            applyViewport(w, if (want == "desktop") DESKTOP_VIEWPORT else null)
            w.reload()
        }
        awaitLoad(snap, NAV_TIMEOUT_MS)
        awaitDomStable(SETTLE_BUDGET_MS)
        val info = pageInfo()
        return ok(
            "已切到${if (want == "desktop") "桌面" else "移动"}版（UA + 视口都换了，页面已重载）",
            info?.optString("url").orEmpty(),
            info?.optString("title").orEmpty(),
        )
    }

    /**
     * 给 View 一个视口。`null` = 手机屏幕那么大（默认）。
     *
     * 尺寸单位是 **CSS px**，要乘 density 变成物理像素 —— WebView 的
     * `innerWidth` 是 `view 宽 / density`，不乘的话页面看到的视口窄一半。
     */
    private fun applyViewport(w: WebView, css: Pair<Int, Int>?) {
        val dm = app.resources.displayMetrics
        // 默认（null）= 手机屏幕，直接用物理像素 —— 别绕一圈 CSS px 再乘回去：
        // 两次取整会把视口缩小一像素（实测 411.4 → 410）
        val pw: Int
        val ph: Int
        if (css == null) {
            pw = dm.widthPixels.coerceAtLeast(320)
            ph = dm.heightPixels.coerceAtLeast(640)
        } else {
            pw = (css.first * dm.density).toInt().coerceAtLeast(320)
            ph = (css.second * dm.density).toInt().coerceAtLeast(320)
        }
        w.measure(
            View.MeasureSpec.makeMeasureSpec(pw, View.MeasureSpec.EXACTLY),
            View.MeasureSpec.makeMeasureSpec(ph, View.MeasureSpec.EXACTLY),
        )
        w.layout(0, 0, pw, ph)
        Log.i(TAG, "视口 ${pw / dm.density}×${ph / dm.density} CSS px（$pw×${ph}px）")
    }

    private fun waitForDomStable(a: JSONObject): JSONObject {
        ensureWeb()
        val seconds = a.optInt("timeout", 10).coerceIn(1, 30)
        val t0 = System.currentTimeMillis()
        awaitDomStable(seconds * 1000L)
        val ms = System.currentTimeMillis() - t0
        val info = pageInfo()
        return ok(
            "等了 ${ms}ms，页面安定（元素 ${info?.optInt("elements") ?: 0} 个）",
            info?.optString("url").orEmpty(),
            info?.optString("title").orEmpty(),
        )
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
     * 截图：两条路，先 GPU 后软件。
     *
     * 1. **GPU 合成**（[HeadlessCapture]）：临时搭一个私有虚拟显示 + 窗口，把
     *    WebView 挂上去让真合成器画一帧、读回来。WebGL / `<video>` 只有这条路
     *    有画面。代价是 200 行和一堆平台坑，所以失败就退回下一条。
     * 2. **软件绘制**（`View.draw`）：不需要窗口、不需要屏幕亮着，DOM/普通图片/
     *    2D canvas 都在，但 **WebGL / `<video>` 是空白**。
     *
     * 退回很常见，不是异常：整页截图超过虚拟显示的单边上限（4096）、面板正挂着
     * 这只 WebView、或者这台设备建不出虚拟显示。结果文本里会写明走的哪条。
     */
    private fun screenshot(fullPage: Boolean = false): JSONObject {
        val w = ensureWeb()
        val dm = app.resources.displayMetrics
        val pxW = w.width
        val baseH = w.height

        // 滚动的补偿：`View.draw(Canvas)` 不做父容器的 translate，
        // 直接画的话内容会往下偏一个 scrollY（顶部空白、底部被裁）。
        // 注意滚动位置在**合成器**里，不挂窗口时 `webView.scrollY` 恒为 0，
        // 所以只能问页面要。
        val scroll = runCatching {
            val a = JSONArray(eval("[Math.round(window.scrollX),Math.round(window.scrollY)]"))
            a.getInt(0) to a.getInt(1)
        }.getOrDefault(0 to 0)

        // 整页截图：把 View 临时拉高到全文档高度（`View.draw` 只画自己的矩形），
        // 画完再恢复。高度封顶 8000px —— 1080×8000 的 ARGB 位图已经 34MB，
        // 再高在手机上就是赌博（而且 Chromium 重新栅格化整页也很费时间）。
        var pxH = baseH
        var clipped = false
        if (fullPage) {
            val cssH = runCatching {
                JSONObject(
                    eval("(function(){return JSON.stringify({h:Math.max(document.documentElement.scrollHeight,document.body.scrollHeight)});})()"),
                ).optInt("h", 0)
            }.getOrDefault(0)
            val want = (cssH * dm.density).toInt()
            pxH = want.coerceIn(baseH, FULL_PAGE_MAX_PX)
            clipped = want > FULL_PAGE_MAX_PX
            if (pxH > baseH) {
                onMain {
                    w.measure(
                        View.MeasureSpec.makeMeasureSpec(pxW, View.MeasureSpec.EXACTLY),
                        View.MeasureSpec.makeMeasureSpec(pxH, View.MeasureSpec.EXACTLY),
                    )
                    w.layout(0, 0, pxW, pxH)
                }
                Thread.sleep(400) // 等重排完再画
            }
        }

        // 先试 **GPU 合成器**（WebGL / `<video>` 只有它有），拿不到再退回软件绘制。
        // 退回很常见：整页截图超过虚拟显示的单边上限、面板正挂着它、
        // 或者这台设备建不出虚拟显示 —— 那些情况以前什么样现在还什么样。
        val gpu = HeadlessCapture.capture(app, w, pxW, pxH)

        val bmp = gpu ?: (onMain {
            val b = Bitmap.createBitmap(pxW, pxH, Bitmap.Config.ARGB_8888)
            val c = Canvas(b)
            c.translate(-(scroll.first * dm.density), -(scroll.second * dm.density))
            w.draw(c)
            b
        } as? Bitmap) ?: return errObj("截图失败")

        // 恢复原尺寸（后续动作还得用正常视口）
        if (pxH != baseH) {
            onMain {
                w.measure(
                    View.MeasureSpec.makeMeasureSpec(pxW, View.MeasureSpec.EXACTLY),
                    View.MeasureSpec.makeMeasureSpec(baseH, View.MeasureSpec.EXACTLY),
                )
                w.layout(0, 0, pxW, baseH)
            }
            runCatching { eval("window.scrollTo(${scroll.first}, ${scroll.second})") }
        }

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
        val kind = if (fullPage) "整页" else "视口"
        val cut = if (clipped) "（页面更长，截到 ${FULL_PAGE_MAX_PX}px 为止）" else ""
        val how = if (gpu != null) "GPU 合成" else "软件绘制"
        return ok(
            "已截图 ${saved.length() / 1024}KB（$kind ${pxW}×${pxH}px，$how$cut）",
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
            active?.dead = true
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
            active?.dead = true
            Log.w(TAG, "主线程 ${timeout}ms 没响应（标记当前标签已坏）")
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
          var list = document.querySelectorAll('${jsQuote(selector)}');
          if (!list.length) return JSON.stringify({error: '没找到元素: ${jsQuote(selector)}'});
          var el = list[0];
          if (el.disabled) return JSON.stringify({error: '元素是 disabled 的'});
          el.scrollIntoView({block: 'center'});
          ${CLICK_EVENTS}
          return JSON.stringify({clicked: true, matched: list.length, tag: el.tagName, text: (el.innerText || el.value || '').substring(0, 100)});
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
          var list = document.querySelectorAll('${jsQuote(selector)}');
          if (!list.length) return JSON.stringify({error: '没找到元素: ${jsQuote(selector)}'});
          var el = list[0];
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
          return JSON.stringify({typed: true, matched: list.length, length: val.length});
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
     * 按键。合成事件不会引发“隐式提交”和“Tab 移焦”，这两件事自己补（见 [press]）。
     */
    private fun pressJs(key: String): String = """
        (function() {
          var name = '${jsQuote(key)}';
          var CODES = {
            Enter: 13, Tab: 9, Escape: 27, Backspace: 8, Delete: 46, ' ': 32,
            ArrowUp: 38, ArrowDown: 40, ArrowLeft: 37, ArrowRight: 39,
            PageUp: 33, PageDown: 34, Home: 36, End: 35
          };
          var code = CODES[name];
          if (code === undefined) return JSON.stringify({error: '不支持的按键: ' + name + '（支持 Enter Tab Escape Backspace Delete Space Arrow* PageUp PageDown Home End）'});
          var el = document.activeElement || document.body;
          function mk(type) {
            var e = new KeyboardEvent(type, {
              key: name, code: name, keyCode: code, which: code,
              bubbles: true, cancelable: true, view: window
            });
            // 很多只想跑在页面里的旧代码读 keyCode/which，而构造函数里给的值
            // 在部分 Chromium 版本上不会真的落到属性上 —— 手动盖一层。
            try {
              Object.defineProperty(e, 'keyCode', {get: function() { return code; }});
              Object.defineProperty(e, 'which', {get: function() { return code; }});
            } catch (err) {}
            return e;
          }
          var target = (el === document.body) ? document : el;
          target.dispatchEvent(mk('keydown'));
          target.dispatchEvent(mk('keypress'));
          target.dispatchEvent(mk('keyup'));

          var acted = '';
          if (name === 'Enter') {
            // 合成 keydown 不会触发表单的隐式提交，得显式来一下：
            // 有的站是自己在 keydown 里处理的（上面那三个事件已经够了），
            // 有的是真表单提交 —— 两条路都盖住。
            var form = el.form || (el.closest && el.closest('form'));
            if (form) {
              try {
                if (typeof form.requestSubmit === 'function') { form.requestSubmit(); acted = '已提交表单'; }
                else {
                  var b = form.querySelector('button[type=submit], input[type=submit]');
                  if (b) { b.click(); acted = '已点提交键'; }
                }
              } catch (err) { acted = '提交表单失败: ' + err.message; }
            }
          }
          if (name === 'Tab') {
            var all = document.querySelectorAll('a[href], button, input, select, textarea, [tabindex]');
            var list = [];
            for (var i = 0; i < all.length; i++) {
              var e2 = all[i];
              if (e2.disabled || e2.tabIndex < 0) continue;
              var st = getComputedStyle(e2);
              if (st.display === 'none' || st.visibility === 'hidden') continue;
              list.push(e2);
            }
            var at = list.indexOf(el);
            if (list.length) {
              var next = list[(at + 1 + list.length) % list.length];
              try { next.focus(); acted = '焦点移到 <' + next.tagName + '>'; } catch (err) {}
            }
          }
          if (name === 'Escape' && document.activeElement && document.activeElement.blur) {
            try { document.activeElement.blur(); acted = '已取消焦点'; } catch (err) {}
          }
          return JSON.stringify({pressed: name, tag: el.tagName, acted: acted});
        })()
    """.trimIndent()

    /**
     * 列出一个 selector 匹配到的元素。
     *
     * 坐标给的是**视口坐标**（`click` 的 x/y 要的正是它）；同时给 pageY，
     * 让模型知道要滚多远。
     */
    private fun findElementsJs(selector: String): String = """
        (function() {
          var all = document.querySelectorAll('${jsQuote(selector)}');
          var sx = window.scrollX || 0, sy = window.scrollY || 0;
          var items = [];
          var limit = Math.min(all.length, 20);
          for (var i = 0; i < limit; i++) {
            var el = all[i];
            var r = el.getBoundingClientRect();
            var st = getComputedStyle(el);
            items.push({
              i: i,
              tag: el.tagName,
              text: (el.innerText || el.value || '').replace(/\s+/g, ' ').trim().substring(0, 80),
              href: (el.href || '').substring(0, 200),
              x: Math.round(r.x + r.width / 2),
              y: Math.round(r.y + r.height / 2),
              w: Math.round(r.width), h: Math.round(r.height),
              pageY: Math.round(r.top + sy),
              visible: r.width > 0 && r.height > 0 && r.bottom > 0 && r.top < window.innerHeight &&
                       st.display !== 'none' && st.visibility !== 'hidden'
            });
          }
          return JSON.stringify({count: all.length, shown: limit, items: items, scrollY: Math.round(sy)});
        })()
    """.trimIndent()

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
     * 抽正文。思路（Readability 的简化版，只留真管用的那几条）：
     *
     * 1. 候选容器：article / main / [role=main] / div / section / td。
     * 2. 打分：里面的 `<p>`（≥ 25 字，不在 nav/footer/aside/header/form 里）
     *    每个算 1 + min(字数/100, 3)；再加一点总长度的分。
     * 3. **链接密度惩罚**：整块里链接文字占比 > 50% 的多半是导航/列表页，×0.3。
     * 4. 输出不取整个容器的 innerText（那还会带上里面的分享按钮、相关阅读），
     *    而是按文档顺序只取 h1-h3 / p / li / blockquote / pre / figcaption，
     *    跳过导航块；块里只有一个链占主体时把链接地址附在后面。
     *
     * 分太低（< 6）就退回 body —— 那就是 `get_text` 的行为，但模型已明确
     * 要求“正文”，总比返回空好。
     */
    private val READABLE_JS = """
        (function() {
          var SKIP_TAGS = {NAV:1, FOOTER:1, ASIDE:1, HEADER:1, FORM:1, SCRIPT:1, STYLE:1, NOSCRIPT:1, IFRAME:1, BUTTON:1, SELECT:1};
          // 块自身的类名像“周边”的：整块不要
          var JUNK = /(^|[\s_-])(nav|menu|sidebar|footer|header|comment|share|sharing|social|promo|ads?|advert|breadcrumb|cookie|consent|privacy|legal|terms|gdpr|ccpa|disclaimer|banner|subscribe|newsletter|signup|login|paywall|related|recommend|popup|modal|toolbar|masthead)([\s_-]|$)/i;
          // 候选容器的正/负信号（Readability 的做法：同一份分数，类名不同差 10 倍）
          var POS = /(^|[\s_-])(article|body|content|entry|main|page|post|story|text|blog|read)([\s_-]|$)/i;
          var NEG = /(^|[\s_-])(nav|menu|sidebar|footer|comment|promo|related|recommend|popular|banner|cookie|consent|privacy|legal|terms|gdpr|subscribe|newsletter|signup|login|paywall|popup|modal|advert|ads?|social|share|widget|tool|tags)([\s_-]|$)/i;
          function t(el) { return (el.innerText || el.textContent || '').trim(); }
          function names(el) {
            return (typeof el.className === 'string' ? el.className : '') + ' ' +
                   (el.id || '') + ' ' + (el.getAttribute ? (el.getAttribute('role') || '') : '');
          }
          function inChrome(el) {
            try { return !!el.closest('nav, footer, aside, header, form, [aria-hidden="true"]'); } catch (e) { return false; }
          }
          // 自己或**上面 6 层**里任何一层像周边，就当周边。
          // 只查自己不够 —— 实测 CNBC 的 cookie 声明是一堆正常 <p>，包在一个
          // `#...CookieNotice` 的壳里，段落自己的类名毫无异常。
          function junk(el) {
            var cur = el, depth = 0;
            while (cur && cur !== document.body && depth++ < 6) {
              if (JUNK.test(names(cur))) return true;
              cur = cur.parentElement;
            }
            return false;
          }
          function hidden(el) {
            var st = getComputedStyle(el);
            return st.display === 'none' || st.visibility === 'hidden';
          }
          function bad(el) { return SKIP_TAGS[el.tagName] || inChrome(el) || hidden(el) || junk(el); }
          function weight(el) {
            var s = names(el);
            if (NEG.test(s)) return 0.15;
            if (POS.test(s)) return 2;
            return 1;
          }

          var h1 = null;
          var hs = document.querySelectorAll('h1');
          for (var i = 0; i < hs.length; i++) {
            if (!bad(hs[i]) && t(hs[i]).length > 3) { h1 = hs[i]; break; }
          }

          var cands = document.querySelectorAll('article, main, [role=main], div, section, td');
          var best = null, bestScore = 0;
          for (var i = 0; i < cands.length; i++) {
            var c = cands[i];
            if (bad(c)) continue;
            var ps = c.querySelectorAll('p');
            if (!ps.length) continue;
            var s = 0;
            for (var j = 0; j < ps.length; j++) {
              var p = ps[j];
              if (bad(p)) continue;
              var pt = t(p);
              if (pt.length < 25) continue;
              s += 1 + Math.min(pt.length / 100, 3);
            }
            if (s <= 0) continue;
            s *= weight(c);
            // 页面主标题在哪个块里 —— 强信号，正文容器几乎一定包含它
            if (h1 && c.contains(h1)) s += 20;
            var all = t(c);
            var linkLen = 0;
            var as = c.querySelectorAll('a');
            for (var k = 0; k < as.length; k++) linkLen += t(as[k]).length;
            if (all.length > 0 && linkLen / all.length > 0.5) s *= 0.3;
            s += Math.min(all.length / 1000, 5);
            if (s > bestScore) { bestScore = s; best = c; }
          }

          var host = best, source = 'best-block';
          if (!host || bestScore < 6) { host = document.body; source = 'body(fallback)'; }

          var chunks = [];
          var nodes = host.querySelectorAll('h1, h2, h3, p, li, blockquote, pre, figcaption');
          for (var i = 0; i < nodes.length; i++) {
            var n = nodes[i];
            if (bad(n)) continue;
            if (n.tagName === 'LI' && n.querySelector('p')) continue;
            var txt = t(n);
            if (!txt || txt.length < 2) continue;
            if (n.tagName === 'PRE') { chunks.push('```\n' + (n.innerText || '') + '\n```'); continue; }
            if (n.tagName === 'H1' || n.tagName === 'H2' || n.tagName === 'H3') { chunks.push('## ' + txt); continue; }
            if (chunks.length > 400) break;
            var links = n.querySelectorAll('a[href]');
            if (links.length === 1 && t(links[0]).length > txt.length * 0.6) {
              try {
                var u = new URL(links[0].href);
                var where = (u.host === location.host) ? (u.pathname + (u.search || '')) : (u.host + u.pathname);
                chunks.push(txt + '  [' + where + ']');
                continue;
              } catch (e) {}
            }
            chunks.push(txt);
          }
          var out = [];
          for (var i = 0; i < chunks.length; i++) {
            if (i > 0 && chunks[i] === chunks[i - 1]) continue;
            out.push(chunks[i]);
          }
          var body = out.join('\n\n');
          var title = document.title || '';
          if (h1) { var ht = t(h1); if (ht && title.indexOf(ht) < 0) title = ht + ' — ' + title; }
          var md = document.querySelector('meta[name="description"]');
          return JSON.stringify({
            text: body.substring(0, 60000),
            length: body.length,
            title: title.substring(0, 200),
            description: md ? (md.content || '').substring(0, 300) : '',
            source: source
          });
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

    /** DOM 指纹：元素个数 + 可见文字长度 + 滚动高度。变了就说明还没稳 */
    private val DOM_SIG_JS =
        "(function(){var b=document.body;if(!b)return 'null';" +
            "return document.querySelectorAll('*').length+':'+(b.innerText||'').length+':'+" +
            "(document.documentElement.scrollHeight||0);})()"
}