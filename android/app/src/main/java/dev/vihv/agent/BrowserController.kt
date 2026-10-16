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

        /** 截图长边上限，和核心 `imaging::MAX_DIM` 一致（超过的那边会再缩一遍） */
        private const val SHOT_MAX_DIM = 2000

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
     * “自己等自己” 10 秒，然后抛异常 → **点一下标签条就闪退**。
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
            pause(60)
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
            val sig = runCatching { eval(domSigJs()) }.getOrNull()
            if (sig == null) return // 页面可能正在换文档（导航），当作“还在动”
            if (sig == last && sig != "null") {
                same++
                if (same >= 3) return
            } else {
                same = 0
            }
            last = sig
            pause(250)
        }
    }

    /** 动作之后的统一“安定”流程：先看它跳没跳，再等 DOM 稳。*/
    private fun settleAfterAction(started0: Int) {
        val navigating = waitForNavStart(started0, 1200)
        if (navigating) awaitLoad(started0, 20_000)
        else pause(SETTLE_MS)
        awaitDomStable(SETTLE_BUDGET_MS)
    }

    /** 等一次新的页面加载开始。`true` = 真的开始加载了 */
    private fun waitForNavStart(started0: Int, maxMs: Long): Boolean {
        val deadline = System.currentTimeMillis() + maxMs
        while (System.currentTimeMillis() < deadline) {
            if (navCounters().first > started0) return true
            pause(60)
        }
        return false
    }

    /** 页面弹过的 alert/confirm/prompt。它们被我们吞掉了，得让模型知道 */
    private val dialogs = ArrayDeque<String>()

    // -----------------------------------------------------------------------
    // 取消
    // -----------------------------------------------------------------------
    //
    // 用户按停止时，Rust 那边丢掉的只是 future —— 这里的阻塞线程还在跑，而且
    // 攥着 [lock]：导航 25s + 等加载 20s + 每次 eval 15s 叠起来能过一分钟，
    // 下一个动作（甚至下一个回合）全排在它后面。所以 Rust 丢掉 future 时会发一个
    // `__cancel`（见 `Core::browser`），这里所有等待都会看它。
    //
    // 用「代」而不是一个布尔：`__cancel` 取消的是**此刻已经进了 [execute]** 的
    // 动作（包括还在排队等锁的）；在它之后才进来的不受影响。

    private class Cancelled : RuntimeException("已取消（回合被停止）")

    private val cancelGen = AtomicInteger()

    /** 当前（持锁的）动作进门时的代。只在锁里写 */
    @Volatile
    private var runningGen = 0

    private fun checkCancelled() {
        if (cancelGen.get() != runningGen) throw Cancelled()
    }

    /** 可取消的 sleep：切成小段，每段之间看一眼有没有被取消 */
    private fun pause(ms: Long) {
        val deadline = System.currentTimeMillis() + ms
        while (true) {
            checkCancelled()
            val left = deadline - System.currentTimeMillis()
            if (left <= 0) return
            Thread.sleep(left.coerceAtMost(50))
        }
    }

    /** 等闩（最多 [timeout]），等的时候也看取消。返回 false = 超时 */
    private fun awaitLatch(latch: CountDownLatch, timeout: Long): Boolean {
        val deadline = System.currentTimeMillis() + timeout
        while (true) {
            checkCancelled()
            val left = deadline - System.currentTimeMillis()
            if (left <= 0) return false
            if (latch.await(left.coerceAtMost(50), TimeUnit.MILLISECONDS)) return true
        }
    }

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

        // 两个内部动作（`__` 开头，模型调不到，见 agent.rs）：不排队、不碰 WebView
        when (action.optString("action")) {
            "__cancel" -> {
                cancelGen.incrementAndGet()
                return ok("已取消").toString()
            }
            // 打开 URI 不需要无障碍：没开无障碍时 `agent open` 走这里（见 http.rs 的 dispatch）
            "__open_uri" -> return try {
                ok(UriLauncher.openFromApp(app, android.net.Uri.parse(action.optString("url")))).toString()
            } catch (t: Throwable) {
                err(t.message ?: t.toString())
            }
        }

        val gen = cancelGen.get()
        lock.withLock {
            runningGen = gen
            busy = true
            return try {
                checkCancelled()
                dispatch(action)
            } catch (c: Cancelled) {
                Log.i(TAG, "动作被取消: ${actionJson.take(200)}")
                err(c.message ?: "已取消")
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
     * agent 正在跑动作时忽略这次点击（tryLock 拿不到就返回 false，面板据此
     * 提示一句）—— 那时的页面是它在操作，半路把当前标签换掉会让它的下一步
     * 落在错的页上。
     */
    fun selectTabFromUi(idx: Int): Boolean {
        onMainThread()
        if (!lock.tryLock()) return false
        try {
            setCurrent(idx)
            refreshAttached()
        } finally {
            lock.unlock()
        }
        return true
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
        // 一只空标签（`list_tabs` 之前先跑 `ensureWeb` 就会每次列完都多一只）。
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
                view?.evaluateJavascript(targetFixJs(), null)
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
                    Log.d(TAG, "[page:$lv] ${m.message()?.take(300)} @${m.lineNumber()}")
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
        JSONObject(eval(pageInfoJs()))
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
     * 一篇 CNBC 报道能拿回 6415 字，一半是菜单和免责声明。这里先找“最像正文的
     * 容器”（段落密度 + 链接密度惩罚），再按文档顺序只取标题/段落/列表项/引用/代码，
     * 顺带去重、去掉导航块。
     */
    private fun getReadable(): JSONObject {
        ensureWeb()
        val out = evalToJson(readableJs()) ?: return errObj("抽正文失败")
        out.optString("error").takeIf { it.isNotEmpty() }?.let { return errObj(it) }

        // 和电脑那边（cdp.rs 的 get_readable）一字不差：同一个动作，模型在两种机器上
        // 看到的该是同一个样子
        val title = out.optString("title")
        var text = out.optString("text")
        val chars = out.optInt("length")
        if (text.length > TEXT_MAX_CHARS) {
            text = text.take(TEXT_MAX_CHARS) + "\n…（已截断）"
        }
        val head = buildString {
            append("正文")
            if (title.isNotEmpty()) append("：【$title】")
            append("（原文 $chars 字，来源 ${out.optString("source")}）")
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
        // 两次取整会把视口缩小一像素（411.4 → 410）
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
        pause(200)
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
        pause(SETTLE_MS)
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
        // 拿回来的是 Promise 对象的序列化，也就是 `{}`。所以把结果写到
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
            pause(120)
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
        //
        // 这一小段（还有下面整页高度的那个）是**截图这套**的私事，不在共享的那份
        // JS 里：桌面走 CDP 的 `Page.captureScreenshot`，压根不需要它。
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
                pause(400) // 等重排完再画
            }
        }

        // 先试 **GPU 合成器**（WebGL / `<video>` 只有它有），拿不到再退回软件绘制。
        // 退回很常见：整页截图超过虚拟显示的单边上限、面板正挂着它、
        // 或者这台设备建不出虚拟显示。
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

        // 长边先缩到 SHOT_MAX_DIM 以内再压：核心发给模型前也会缩到这个尺寸，
        // 这里不缩的话那边就得把 JPEG 整个解码、缩放、再编码一遍（手机上几百毫秒）。
        // 缩好了它直接原样放行。
        val scale = SHOT_MAX_DIM.toFloat() / maxOf(bmp.width, bmp.height)
        val out = if (scale < 1f) {
            Bitmap.createScaledBitmap(
                bmp,
                (bmp.width * scale).toInt().coerceAtLeast(1),
                (bmp.height * scale).toInt().coerceAtLeast(1),
                true,
            )
        } else {
            bmp
        }
        // 下面 finally 里会 recycle，尺寸先记下来
        val outW = out.width
        val outH = out.height
        val saved: File? = try {
            val dir = File(RootfsInstaller.rootfsDir(app), "root/.browser")
            if (!dir.exists() && !dir.mkdirs()) error("建不了目录 $dir")
            val f = File(dir, "shot-${System.currentTimeMillis()}.jpg")
            f.outputStream().use { out.compress(Bitmap.CompressFormat.JPEG, SHOT_QUALITY, it) }
            f
        } catch (t: Throwable) {
            Log.w(TAG, "截图落盘失败", t)
            null
        } finally {
            if (out !== bmp) out.recycle()
            bmp.recycle()
        }
        if (saved == null) return errObj("截图存不下来（沙箱目录写不进？）")

        val guest = "$SHOT_GUEST_DIR/${saved.name}"
        val info = pageInfo()
        val kind = if (fullPage) "整页" else "视口"
        val cut = if (clipped) "（页面更长，截到 ${FULL_PAGE_MAX_PX}px 为止）" else ""
        val how = if (gpu != null) "GPU 合成" else "软件绘制"
        val size = if (scale < 1f) "${pxW}×${pxH}px → ${outW}×${outH}" else "${pxW}×${pxH}px"
        return ok(
            "已截图 ${saved.length() / 1024}KB（$kind $size，$how$cut）",
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
        if (!awaitLatch(latch, timeout)) {
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
            // 现象是模型那一轮报「建不出 WebView」、栈在 ensureWeb，
            // 而且是 3ms 就返回（不是超时）—— 就是输在这个竞态上。
            try {
                out = block()
            } catch (t: Throwable) {
                Log.w(TAG, "主线程操作失败", t)
            } finally {
                latch.countDown()
            }
        }
        if (!awaitLatch(latch, timeout)) {
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
    // JS：**不在这个文件里**
    //
    // 动作级的页面逻辑（骨架、正文、点击、输入、按键、滚动、找元素、DOM 指纹、
    // 页面信息、`target=_blank` 修正）只有**一份**，在核心里
    // （`core/src/browser_js.rs`）—— 桌面那个 CDP 后端用的是同一个 `build`。
    // 各写一份就会漂：动作表漂过一次（`new_tab` 把 URL 吃掉），而 JS 漂了更隐蔽
    // （点不动、抽不出正文），只有真机上才看得出来。
    //
    // 这边只剩**宿主逻辑**：往哪只 WebView 里注、什么时候注、超时怎么办。
    // -----------------------------------------------------------------------

    /** 向核心要一段页面 JS（kind 见 `browser_js::KINDS`，params 是 JSON 对象）*/
    private fun js(kind: String, params: String = "{}"): String =
        runCatching { AgentApp.core.browserJs(kind, params) }
            .getOrElse { e ->
                // 拿不到就把原因说清楚：这个动作做不了，而不是静默注入一段空脚本
                Log.e(TAG, "取 JS 失败 kind=$kind", e)
                "throw new Error('取页面脚本失败: ${e.message}')"
            }

    private fun clickSelJs(selector: String) =
        js("click_sel", """{"selector":${JSONObject.quote(selector)}}""")
    private fun clickXyJs(x: Int, y: Int) = js("click_xy", """{"x":$x,"y":$y}""")
    private fun typeJs(selector: String, text: String) =
        js("type", """{"selector":${JSONObject.quote(selector)},"text":${JSONObject.quote(text)}}""")
    private fun getTextJs(selector: String?) =
        js("get_text", """{"selector":${JSONObject.quote(selector ?: "")}}""")
    private fun scrollJs(direction: String, amount: Int, selector: String?) =
        js("scroll", """{"direction":${JSONObject.quote(direction)},"amount":$amount,"selector":${JSONObject.quote(selector ?: "")}}""")
    private fun pressJs(key: String) = js("press", """{"key":${JSONObject.quote(key)}}""")
    private fun findElementsJs(selector: String) = js("find_elements", """{"selector":${JSONObject.quote(selector)}}""")
    private fun backboneJs(maxDepth: Int) = js("backbone", """{"max_depth":$maxDepth}""")
    private fun readableJs() = js("readable")
    private fun targetFixJs() = js("target_fix")
    private fun domSigJs() = js("dom_sig")
    private fun pageInfoJs() = js("page_info")

    /** [runJs] 踢出去的那一脚的结果。`pending:1` = 还没结束 —— 这个**是宿主逻辑**
     *  （`evaluateJavascript` 不会等 Promise，所以只能踢一脚再轮询），不属于共享
     *  的那份 JS。桌面那边用 CDP 的 `awaitPromise`，根本不需要它。 */
    private val POLL_JS = """
        (function() {
          var v = window.__agentJs;
          if (v === null || v === undefined) return JSON.stringify({pending: 1});
          if (v.err) return JSON.stringify({pending: 0, err: String(v.err)});
          return JSON.stringify({pending: 0, ok: (typeof v.ok === 'string' ? v.ok : JSON.stringify(v.ok))});
        })()
    """.trimIndent()
}