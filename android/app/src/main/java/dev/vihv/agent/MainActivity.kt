package dev.vihv.agent

import android.Manifest
import android.app.Activity
import android.content.ContentValues
import android.content.Intent
import android.graphics.Color
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.MediaStore
import android.provider.Settings
import android.util.Log
import android.view.View
import android.view.ViewGroup
import android.webkit.JavascriptInterface
import android.webkit.ValueCallback
import android.webkit.WebChromeClient
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.FrameLayout
import android.widget.TextView

/**
 * WebView 壳（决策 D3）。
 *
 * 加载 `http://127.0.0.1:<port>` —— 前端和 API **同源**，所以：
 * - 不需要 CORS
 * - 是 secure context，`crypto.subtle` / Service Worker 都能用
 * - 开发期只要把 Vite 的地址换上去就能热更新
 *
 * 明文流量由 `res/xml/network_security_config.xml` 给 loopback 开了例外，
 * 否则 Android 9+ 会直接报 ERR_CLEARTEXT_NOT_PERMITTED。
 */
class MainActivity : Activity() {

    companion object {
        private const val TAG = "AgentMain"
        private const val PREFS = "agent"
        private const val KEY_ASKED_A11Y = "asked_accessibility"
        private const val KEY_ASKED_FILES = "asked_all_files"
        private const val KEY_ASKED_NOTIF = "asked_notifications"
        private const val REQ_STORAGE = 2
        /** 页面 `<input type=file>` 选文件 */
        private const val REQ_FILE = 3
        /** 页面 `<input capture>` 拉起相机 */
        private const val REQ_CAMERA = 4
        /** Android 13+ 的通知权限（回合进行中的前台服务要用）*/
        private const val REQ_NOTIF = 5

        /**
         * 当前活着的 Activity。
         *
         * 只给一件事用：回合开始时（Rust 从后台线程通知过来）问一次通知权限 ——
         * 权限弹窗只能从 Activity 发，而那是唯一一个“人就在看着”的时机。
         * `singleTask` + 只存一个，所以不用担心泄漏（onPause 就清）。
         */
        @Volatile
        var current: MainActivity? = null
            private set
    }

    private var web: WebView? = null
    /** 已注册的系统返回回调（API 33+；类型用 Any 存，低版本设备不会碰到这个类） */
    private var backCallback: Any? = null
    /** 页面等着回调的文件选择请求（WebView 的 onShowFileChooser） */
    private var fileCallback: ValueCallback<Array<Uri>>? = null
    /** 相机拍下的那张图落在 MediaStore 里的 URI（取消时要删掉） */
    private var cameraUri: Uri? = null

    // —— 浏览器看板（agent 那只无头 WebView 的观察窗口）——
    //
    // 为什么是**原生** View 而不是页面里的一块 HTML：要显示的是另一只真 WebView，
    // 而 WebView 不能嵌在网页里。所以这一层盖在 UI WebView 上面，需要时把 agent
    // 那只 WebView `addView` 进来，关掉再摘下去 —— **同一只**，不另建，
    // 否则用户看到的和 agent 操作的不是同一个页面（见 docs/08）。
    private var browserPanel: FrameLayout? = null
    private var browserHolder: FrameLayout? = null
    private var browserTitle: TextView? = null
    private var browserUrl: TextView? = null
    private var browserState: TextView? = null
    /** 标签条：只有多于一只标签时才显示 */
    private var tabChips: android.widget.LinearLayout? = null
    /** 标签条的签名（标签短名 + 当前下标）。没变就不重建 */
    private var chipsSig = ""
    private var panelVisible = false
    /** 面板开着时注册的返回回调（优先级比页面那层高） */
    private var panelBack: Any? = null
    private val panelTick = android.os.Handler(android.os.Looper.getMainLooper())

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        edgeToEdge()

        WebView.setWebContentsDebuggingEnabled(isDebuggable())

        val port = AgentApp.port
        if (port == 0) {
            setContentView(errorView("核心没起来。看 logcat 里 AgentApp 的报错。"))
        } else {
            setContentView(buildRoot(buildWebView(port)))
        }

        // 头部的地球按钮只在 agent 真开着浏览器时才显示。状态在壳这边
        // （`AgentApp.browser.hasPage()`），所以由壳推给页面；页面加载时
        // 还会自己问一次（见 `AgentHost.hasBrowser`）。
        // `onDestroy` 里要把它置回 null：浏览器比 Activity 活得久。
        AgentApp.browser?.onPagesChanged = { pushBrowserAvailable() }

        // 两个设置页只能一个一个来：先存储（沙箱干活的地方），下次启动再无障碍
        if (!maybeAskAllFilesAccess()) maybeOpenAccessibilitySettings()

        // 冷启动也认这个 extra（见 onNewIntent）
        if (intent?.getBooleanExtra("browser", false) == true) {
            window.decorView.post { showBrowserPanel() }
        }
    }

    override fun onResume() {
        super.onResume()
        current = this
        // 从设置页回来时权限可能刚打开：重挂沙箱的 /sdcard 和 /workspace
        AgentApp.syncSharedStorage(this)
    }

    /**
     * `adb shell am start -n dev.vihv.agent/.MainActivity --ez browser true`
     * 直接打开浏览器看板。
     *
     * 为什么要留这个口子：看板是给**人**看的，但它自己（头部那个地球按钮）
     * 只能用手点 —— 而调试/验收时不该去戳用户正在用的手机的屏幕。
     * 和 `files/port`、`agentctl` 同一个思路：能在 adb 里做的事就不靠摸屏幕。
     * 没开过页面时它只会提示一句（和点地球一样）。
     */
    override fun onNewIntent(intent: Intent?) {
        super.onNewIntent(intent)
        if (intent?.getBooleanExtra("browser", false) == true) showBrowserPanel()
    }

    override fun onPause() {
        if (current === this) current = null
        super.onPause()
    }

    /**
     * 回合开始时问一次通知权限（Android 13+）。
     *
     * 为什么是**回合开始时**而不是启动时：这个权限只为「回合进行中的通知」存在，
     * 那时弹出来用户能立刻看见后果（通知栏上就有一条），不至于莫名其妙。
     * 只问一次，拒了就不再扰（服务照样跑，只是看不见那条通知）。
     */
    fun maybeAskNotificationPermission() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) ==
            android.content.pm.PackageManager.PERMISSION_GRANTED
        ) {
            return
        }
        val prefs = getSharedPreferences(PREFS, MODE_PRIVATE)
        if (prefs.getBoolean(KEY_ASKED_NOTIF, false)) return
        prefs.edit().putBoolean(KEY_ASKED_NOTIF, true).apply()
        runCatching { requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), REQ_NOTIF) }
            .onFailure { Log.w(TAG, "申请通知权限失败", it) }
    }

    /**
     * 让内容铺到状态栏/导航栏底下。
     *
     * 这样页面里的 `env(safe-area-inset-*)` 才是真实值 —— 顶部/底部的
     * 留白由系统给，而不是前端猜一个魔法数字。换机型、换刘海、换手势条
     * 都不用改 CSS。
     *
     * 不用 AndroidX：`WindowCompat` 会拉进一堆依赖，平台 API 就够了。
     */
    private fun edgeToEdge() {
        @Suppress("DEPRECATION")
        run {
            window.statusBarColor = Color.TRANSPARENT
            window.navigationBarColor = Color.TRANSPARENT
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window.setDecorFitsSystemWindows(false)
        } else {
            @Suppress("DEPRECATION")
            window.decorView.systemUiVisibility =
                View.SYSTEM_UI_FLAG_LAYOUT_STABLE or View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
        }
        // 深色背景 → 状态栏图标保持亮色。默认就是这个，显式写一下防止被系统改。
        //
        // ⚠️ 必须包起来：`window.insetsController` 在 **decor view 还没建出来**时
        // 会 NPE —— 不是返回 null，而是它内部 `mDecor.getWindowInsetsController()`
        // 直接抛（PhoneWindow.getInsetsController），所以 `?.` 拦不住。
        // 实测 API 35 模拟器上 onCreate 里调到这行就崩、**整个进程跟着死**
        // （真机 API 36 上碰巧没崩，所以一直没发现）。
        // 它只管图标亮不亮，崩掉不值得。
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            runCatching {
                window.insetsController?.setSystemBarsAppearance(
                    0,
                    android.view.WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS,
                )
            }.onFailure { Log.w(TAG, "设置状态栏外观失败（decor 还没建好？）", it) }
        }
    }

    /**
     * 根容器：UI 的 WebView + 盖在它上面的浏览器看板。
     *
     * 看板平时 `GONE`，所以常态下这层包装不花任何代价。
     */
    private fun buildRoot(ui: WebView): View {
        val root = FrameLayout(this)
        root.addView(
            ui,
            FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT,
            ),
        )
        val panel = buildBrowserPanel()
        browserPanel = panel
        root.addView(
            panel,
            FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT,
            ),
        )
        return root
    }

    /**
     * 浏览器看板。头一行放标题/URL/agent 状态 + 关闭，下面全是给 agent 那只
     * WebView 的位置。
     *
     * 尺寸单位刻意用 `dp()`（不是 px）：这里量的是手机屏幕，不是网页。
     */
    private fun buildBrowserPanel(): FrameLayout {
        val panel = FrameLayout(this).apply {
            setBackgroundColor(getColor(R.color.ink))
            visibility = View.GONE
        }

        val bar = android.widget.LinearLayout(this).apply {
            orientation = android.widget.LinearLayout.HORIZONTAL
            gravity = android.view.Gravity.CENTER_VERTICAL
            setPadding(dp(16), dp(8), dp(8), dp(8))
        }
        val col = android.widget.LinearLayout(this).apply {
            orientation = android.widget.LinearLayout.VERTICAL
        }
        browserTitle = TextView(this).apply {
            setTextColor(getColor(R.color.fg))
            textSize = 15f
            maxLines = 1
            ellipsize = android.text.TextUtils.TruncateAt.END
            text = "agent 的浏览器"
        }
        browserUrl = TextView(this).apply {
            setTextColor(getColor(R.color.muted))
            textSize = 11f
            maxLines = 1
            ellipsize = android.text.TextUtils.TruncateAt.END
        }
        col.addView(browserTitle)
        col.addView(browserUrl)

        browserState = TextView(this).apply {
            setTextColor(getColor(R.color.muted))
            textSize = 11f
            setPadding(dp(8), 0, dp(8), 0)
        }
        val close = TextView(this).apply {
            text = "关闭"
            setTextColor(getColor(R.color.muted))
            textSize = 13f
            setPadding(dp(12), dp(8), dp(12), dp(8))
            setOnClickListener { hideBrowserPanel() }
        }
        bar.addView(
            col,
            android.widget.LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f),
        )
        bar.addView(browserState)
        bar.addView(close)

        val divider = View(this).apply { setBackgroundColor(getColor(R.color.hairline)) }
        // 标签条：多标签时才出现（一只的时候它没有信息量）
        tabChips = android.widget.LinearLayout(this).apply {
            orientation = android.widget.LinearLayout.HORIZONTAL
            visibility = View.GONE
            setPadding(dp(12), dp(6), dp(12), dp(6))
        }
        browserHolder = FrameLayout(this)

        val column = android.widget.LinearLayout(this).apply {
            orientation = android.widget.LinearLayout.VERTICAL
        }
        column.addView(bar)
        column.addView(divider, android.widget.LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 1))
        column.addView(tabChips)
        column.addView(
            browserHolder,
            android.widget.LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f),
        )
        panel.addView(
            column,
            FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT,
            ),
        )

        // 边到边：头要让开状态栏，底部让开手势条（和网页里的
        // env(safe-area-inset-*) 是同一份信息，只是这边拿不到 CSS 变量）
        panel.setOnApplyWindowInsetsListener { _, insets ->
            applyPanelInsets(insets)
            insets
        }
        return panel
    }

    private fun applyPanelInsets(insets: android.view.WindowInsets? = null) {
        // 三个来源依次回退：参数 → 面板自己的 → Activity 的。
        // 面板 `GONE` 的时候 `rootWindowInsets` 可能是 null，所以必须有
        // 最后一层 —— 否则头部会钻到状态栏下面去。
        val i = insets ?: browserPanel?.rootWindowInsets ?: window.decorView.rootWindowInsets ?: return
        val top: Int
        val bottom: Int
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val bars = i.getInsets(android.view.WindowInsets.Type.systemBars())
            top = bars.top
            bottom = bars.bottom
        } else {
            @Suppress("DEPRECATION")
            run {
                top = i.systemWindowInsetTop
                bottom = i.systemWindowInsetBottom
            }
        }
        // 头本就自带上内边距，只把状态栏那一段加上去
        (browserPanel?.getChildAt(0) as? android.widget.LinearLayout)?.let { column ->
            column.setPadding(0, top, 0, 0)
        }
        browserHolder?.setPadding(0, 0, 0, bottom)
    }

    /**
     * 告诉页面「agent 有没有开着浏览器」—— 头部那个地球按钮据此显隐。
     *
     * 页面加载时会自己问一次（`AgentHost.hasBrowser()`），这里管的是**页面活着
     * 的时候状态变了**（agent 第一次开页面 / 把标签关光）。
     */
    private fun pushBrowserAvailable() {
        val on = AgentApp.browser?.hasPage() == true
        web?.evaluateJavascript(
            "window.__browserAvailable && window.__browserAvailable($on)",
            null,
        )
    }

    /** 打开看板。没开过页面就不开（没东西可看）。 */
    private fun showBrowserPanel() {
        val panel = browserPanel ?: return
        val holder = browserHolder ?: return
        val browser = AgentApp.browser
        if (browser == null || !browser.hasPage()) {
            android.widget.Toast.makeText(this, "agent 还没打开过网页", android.widget.Toast.LENGTH_SHORT)
                .show()
            return
        }
        if (panelVisible) return
        panelVisible = true
        browser.attachTo(holder)
        applyPanelInsets()
        panel.visibility = View.VISIBLE
        // 从屏幕外滑上来：和页面里的面板同一个手感（in/out 都用位移）
        panel.translationY = resources.displayMetrics.heightPixels.toFloat()
        panel.animate().translationY(0f).setDuration(200).start()
        // 刚变可见时 insets 可能还没派发过，补一次
        panel.post { applyPanelInsets() }
        registerPanelBack()
        panelTick.post(pollPanel)
        Log.i(TAG, "打开浏览器看板")
    }

    private fun hideBrowserPanel() {
        val panel = browserPanel ?: return
        if (!panelVisible) return
        panelVisible = false
        panelTick.removeCallbacks(pollPanel)
        unregisterPanelBack()
        val off = panel.height.toFloat().coerceAtLeast(
            resources.displayMetrics.heightPixels.toFloat(),
        )
        panel.animate().translationY(off).setDuration(180).withEndAction {
            panel.visibility = View.GONE
            // 摘下来：agent 接着在无头模式辇（不重载、状态不丢）
            AgentApp.browser?.detach()
        }.start()
        Log.i(TAG, "收起浏览器看板")
    }

    /** 每半秒刷新头的标题/URL/状态。面板不开时不跑。 */
    private val pollPanel = object : Runnable {
        override fun run() {
            if (!panelVisible) return
            val st = AgentApp.browser?.pageState()
            if (st != null) {
                browserTitle?.text = st.title.ifBlank { "agent 的浏览器" }
                browserUrl?.text = st.url
                browserState?.text = if (st.busy) "agent 正在操作…" else "空闲"
                syncTabChips(st)
            }
            panelTick.postDelayed(this, 500)
        }
    }

    /**
     * 标签条。只在「标签数 + 当前下标」变了才重建 —— 这个函数每 500ms 被调一次，
     * 每次都 removeAllViews 会让用户点中的那颗按钮在手指底下消失。
     */
    private fun syncTabChips(st: BrowserController.PanelState) {
        val row = tabChips ?: return
        val sig = st.labels.joinToString("|") + "#" + st.currentIdx
        if (sig == chipsSig) return
        chipsSig = sig

        row.removeAllViews()
        if (st.labels.size <= 1) {
            row.visibility = View.GONE
            return
        }
        row.visibility = View.VISIBLE
        st.labels.forEachIndexed { i, label ->
            val on = i == st.currentIdx
            val chip = TextView(this).apply {
                text = "${i + 1} $label"
                textSize = 11f
                maxLines = 1
                ellipsize = android.text.TextUtils.TruncateAt.END
                gravity = android.view.Gravity.CENTER
                setPadding(dp(8), dp(5), dp(8), dp(5))
                setTextColor(getColor(if (on) R.color.fg else R.color.muted))
                background = android.graphics.drawable.GradientDrawable().apply {
                    cornerRadius = dp(9).toFloat()
                    setColor(getColor(if (on) R.color.hairline else android.R.color.transparent))
                }
                setOnClickListener { switchTabFromUi(i) }
            }
            row.addView(
                chip,
                android.widget.LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
                    .apply { marginEnd = dp(4) },
            )
        }
    }

    /** 面板上点了某只标签。agent 正在跑动作时它会被忽略（见 selectTabFromUi）。 */
    private fun switchTabFromUi(i: Int) {
        AgentApp.browser?.selectTabFromUi(i)
        chipsSig = "" // 强制下一拍重建（高亮要立刻跟上）
    }

    private fun registerPanelBack() {
        if (panelBack != null || Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        val cb = android.window.OnBackInvokedCallback { hideBrowserPanel() }
        @Suppress("NewApi")
        onBackInvokedDispatcher.registerOnBackInvokedCallback(
            android.window.OnBackInvokedDispatcher.PRIORITY_OVERLAY,
            cb,
        )
        panelBack = cb
    }

    private fun unregisterPanelBack() {
        val cb = panelBack ?: return
        panelBack = null
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        @Suppress("NewApi")
        onBackInvokedDispatcher.unregisterOnBackInvokedCallback(
            cb as android.window.OnBackInvokedCallback,
        )
    }

    private fun dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()

    private fun buildWebView(port: Int): WebView {
        val w = WebView(this)
        w.layoutParams = FrameLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.MATCH_PARENT,
        )
        // 和窗口背景、页面背景同色（@color/ink）。原来是纯黑：启动时先闪一下
        // 黑，页面画出来再变成 #151515。
        w.setBackgroundColor(getColor(R.color.ink))

        w.settings.apply {
            javaScriptEnabled = true
            domStorageEnabled = true
            // 前端不做缩放，交给 CSS
            setSupportZoom(false)
            builtInZoomControls = false
            mediaPlaybackRequiresUserGesture = false
            cacheMode = WebSettings.LOAD_DEFAULT
            // 允许混合内容不是必需的（我们全程 http loopback），保持默认
            mixedContentMode = WebSettings.MIXED_CONTENT_NEVER_ALLOW
        }

        // 页面 → 壳的一根细线：告诉壳「现在有覆盖层要接返回键」。
        //
        // 为什么不直接看 web.canGoBack()：注册了系统返回回调，退出 App 那个
        // 手势的动画就得我们画（否则屏幕上什么都不动）；所以只有真的有覆盖层
        // 时才注册，其余时候把返回完全交给系统。
        // 只会暴露给顶层页面（App 自己）；查看器里的 iframe 是 opaque origin，
        // 够不到这个对象。
        w.addJavascriptInterface(BackHost(), "AgentHost")

        w.webViewClient = object : WebViewClient() {
            /**
             * 外部链接交给系统浏览器。
             *
             * 默认行为是在 WebView 里直接打开 —— 点一下消息里的链接，整个 App
             * 界面就被那个网页替换掉了，只能按返回键回来。只有我们自己的
             * 127.0.0.1 留在 WebView 里。
             */
            override fun shouldOverrideUrlLoading(
                view: WebView?,
                request: android.webkit.WebResourceRequest?,
            ): Boolean {
                val uri = request?.url ?: return false
                if (uri.host == "127.0.0.1" && uri.port == port) return false
                runCatching { startActivity(Intent(Intent.ACTION_VIEW, uri)) }
                    .onFailure { Log.w(TAG, "打不开链接 $uri", it) }
                return true
            }

            override fun onPageFinished(view: WebView?, url: String?) {
                // 页面自己挂载时也会问一次，这里防的是「状态刚好在页面加载那
                // 一下变了」：补推一次，删掉也不会错，只是少一层保险。
                pushBrowserAvailable()
            }

            override fun onReceivedError(
                view: WebView?,
                request: android.webkit.WebResourceRequest?,
                error: android.webkit.WebResourceError?,
            ) {
                // 只关心主文档的失败，子资源失败不该盖掉页面
                if (request?.isForMainFrame == true) {
                    Log.e(TAG, "主文档加载失败: ${error?.description}")
                    setContentView(
                        errorView(
                            "加载 http://127.0.0.1:$port 失败\n${error?.description}\n\n" +
                                "最常见原因：network_security_config 没放行 loopback 明文。",
                        ),
                    )
                }
            }
        }

        w.webChromeClient = object : WebChromeClient() {
            /**
             * 页面上 `<input type=file>` 的点击要靠这里接住。
             *
             * WebView **默认什么都不做** —— 不实现这个方法，附件的「相机 /
             * 照片 / 文件」哪一颗都是点了没反应（而且不报错）。
             *
             * 三颗入口在页面里对应三个 input：
             *   - 相机：`capture="environment"` → `isCaptureEnabled` 为真，
             *     直接拉 `ACTION_IMAGE_CAPTURE`（不经过文件选择器）
             *   - 照片 / 文件：`ACTION_OPEN_DOCUMENT`（见 `chooserIntent()`）
             */
            override fun onShowFileChooser(
                webView: WebView?,
                filePathCallback: ValueCallback<Array<Uri>>?,
                fileChooserParams: FileChooserParams?,
            ): Boolean {
                if (fileChooserParams == null) return false
                // 上一个请求还没回落就再点一次：先把它取消，否则页面永远拿不到回调
                fileCallback?.onReceiveValue(null)
                fileCallback = filePathCallback

                if (fileChooserParams.isCaptureEnabled) {
                    val intent = cameraIntent()
                    if (intent != null) {
                        val ok = runCatching { startActivityForResult(intent, REQ_CAMERA) }
                            .onFailure { Log.w(TAG, "相机不可用，回退到文件选择", it) }
                            .isSuccess
                        if (ok) return true
                    }
                }

                Log.i(
                    TAG,
                    "文件选择：accept=${fileChooserParams.acceptTypes.joinToString()} " +
                        "multiple=${fileChooserParams.mode == FileChooserParams.MODE_OPEN_MULTIPLE}",
                )
                return try {
                    startActivityForResult(chooserIntent(fileChooserParams), REQ_FILE)
                    true
                } catch (e: Exception) {
                    Log.w(TAG, "打开文件选择失败", e)
                    fileCallback = null
                    false
                }
            }

            override fun onConsoleMessage(m: android.webkit.ConsoleMessage?): Boolean {
                Log.d(TAG, "[web] ${m?.message()} @${m?.lineNumber()}")
                return true
            }
        }

        // 先过 /auth：核心种上 HttpOnly cookie，再跳回首页。之后页面的
        // fetch / WebSocket / <img> 都自动带着凭据，页面里的 JS 也读不到它。
        val t = AgentApp.token
        w.loadUrl(
            if (t != null) "http://127.0.0.1:$port/auth?t=$t" else "http://127.0.0.1:$port",
        )
        web = w
        return w
    }

    /**
     * 相机：把输出直接写进 MediaStore，换一张我们自己的 `content://` URI。
     *
     * 为什么不用 `data` 里的小缩略图：那只有几十 KB，看不清、也不能给模型看。
     * 为什么不用文件路径 + `file://`：targetSdk 24+ 对一个外部的 `file://` 直接
     * 抛 `FileUriExposedException`。MediaStore 的 content URI 两条都避开了，
     * 也不像 FileProvider 那样要再引一层 AndroidX。
     *
     * 返回 null 表示插不进 MediaStore（极罕见），让调用方回退到文件选择器。
     */
    private fun cameraIntent(): Intent? {
        val values = ContentValues().apply {
            put(MediaStore.Images.Media.DISPLAY_NAME, "agent-${System.currentTimeMillis()}.jpg")
            put(MediaStore.Images.Media.MIME_TYPE, "image/jpeg")
        }
        val uri = runCatching {
            contentResolver.insert(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, values)
        }.getOrNull() ?: return null
        cameraUri = uri
        return Intent(MediaStore.ACTION_IMAGE_CAPTURE).apply {
            putExtra(MediaStore.EXTRA_OUTPUT, uri)
            addFlags(Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
        }
    }

    /**
     * 文件选择器的回流。两个 requestCode：
     * - `REQ_CAMERA`：`EXTRA_OUTPUT` 的 URI 不会出现在 `data` 里，所以直接
     *   把我们自己插的那张交回去；取消则把空图删掉（否则相册里会多一张
     *   永远没人用的照片）。
     * - `REQ_FILE`：交给 `FileChooserParams.parseResult`，它会把单选/多选、
     *   `content://` 的可持久化授权都处理好。
     */
    @Deprecated("平台接口；我们不用 AndroidX，所以走老的 onActivityResult")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        val cb = fileCallback ?: return
        when (requestCode) {
            REQ_CAMERA -> {
                val uri = cameraUri
                cameraUri = null
                if (resultCode == RESULT_OK && uri != null) {
                    cb.onReceiveValue(arrayOf(uri))
                } else {
                    uri?.let { runCatching { contentResolver.delete(it, null, null) } }
                    cb.onReceiveValue(null)
                }
                fileCallback = null
            }
            REQ_FILE -> {
                fileCallback = null
                // 不拿可持久化授权（takePersistableUriPermission）：页面在这次回调里
                // 就把文件读走、传给核心了，用不着跨重启的权限。原来每选一次就攒
                // 一条，系统那边的授权表只增不减。
                val uris = urisFromChooserResult(resultCode, data)
                Log.i(TAG, "文件选择返回 resultCode=$resultCode uris=${uris?.joinToString()}")
                cb.onReceiveValue(uris)
            }
        }
    }

    /**
     * 从系统选择器的结果里抠出选中的 URI。
     *
     * **不能直接用 `WebChromeClient.FileChooserParams.parseResult`**：实测
     * Pixel 10 / Android 16 的 Google 相册选择器（它接住了
     * `ACTION_GET_CONTENT`）返回时 `getData()` 和 `getClipData()` **都是 null**，
     * `parseResult` 于是返回 null → WebView 拿不到文件，`<input>` 的 change
     * 事件根本不触发，界面上就像“选了但没加进去”。
     *
     * 所以这里把常见几种携带形式都扫一遍：clipData → data → 几个已知的
     * extra。都拿不到再把整个 Intent 打进日志，下次碰到别的选择器能直接看。
     */
    private fun urisFromChooserResult(resultCode: Int, data: Intent?): Array<Uri>? {
        if (resultCode != RESULT_OK) {
            Log.i(TAG, "选择器取消（resultCode=$resultCode）")
            return null
        }
        if (data == null) {
            Log.w(TAG, "选择器返回 RESULT_OK 但 data 是 null")
            return null
        }

        val out = LinkedHashSet<Uri>()
        data.clipData?.let { cd ->
            for (i in 0 until cd.itemCount) cd.getItemAt(i).uri?.let { out.add(it) }
        }
        data.data?.let { out.add(it) }
        // 有些实现把 URI 塞在 extra 里（EXTRA_STREAM 可能是单个 Uri，也可能是
        // ArrayList<Uri>）。类型对不上会抛，包起来。
        runCatching { data.getParcelableExtra<Uri>(Intent.EXTRA_STREAM) }
            .getOrNull()?.let { out.add(it) }
        @Suppress("DEPRECATION")
        runCatching { data.getParcelableArrayListExtra<Uri>(Intent.EXTRA_STREAM) }
            .getOrNull()?.forEach { out.add(it) }

        if (out.isEmpty()) {
            val keys = data.extras?.keySet()?.joinToString() ?: "—"
            Log.w(
                TAG,
                "选择器返回里没有 URI：action=${data.action} data=${data.data} " +
                    "clip=${data.clipData?.itemCount ?: 0} extras=[$keys]",
            )
            return null
        }
        return out.toTypedArray()
    }

    /**
     * 照片 / 文件选择器。
     *
     * **刻意不用 `FileChooserParams.createIntent()`**：它发的是
     * `ACTION_GET_CONTENT`，在 Android 13+ 上会命中系统 Photo Picker
     * （`PhotopickerGetContentActivity`）。实测 Pixel 10 / Android 16 上
     * 选完照片点「完成」回来，`WebChromeClient.FileChooserParams.parseResult`
     * 拿不到任何 URI（`uris=0`）→ WebView 的 `<input>` 不触发 change，
     * 界面上就像「选了但没加进去」。（Photo Picker 还有另一个已知坑：
     * 它返回的 proxy URI 会让 Chromium 报 `ERR_UPLOAD_FILE_CHANGED`。
     * WordPress/GutenbergKit 也因为这个从 GET_CONTENT 换到 OPEN_DOCUMENT。）
     *
     * `ACTION_OPEN_DOCUMENT`（SAF / 文件应用）的结果契约稳定：单选走
     * `intent.data`、多选走 `intent.clipData`，而且支持可持久化读权限。
     * 代价是 UI 是「文件」而不是那个照片网格 —— 换可靠性值得。
     */
    private fun chooserIntent(params: WebChromeClient.FileChooserParams): Intent {
        // acceptTypes 里可能有 `.pdf` 这种非 MIME 写法，DocumentsUI 会不认
        val mimes = params.acceptTypes.filter { it.contains('/') }
        val multiple = params.mode == WebChromeClient.FileChooserParams.MODE_OPEN_MULTIPLE

        // 只要选的是图片，优先走系统 Photo Picker（相册那个网格，就是 Claude 的
        // 「照片」打开的东西）。它是 `ACTION_PICK_IMAGES`，结果契约清楚：
        // 单选 `intent.data`、多选 `intent.clipData`，正是 `urisFromChooserResult`
        // 扫的那两处。
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            mimes.isNotEmpty() && mimes.all { it.startsWith("image/") }
        ) {
            val pick = Intent(MediaStore.ACTION_PICK_IMAGES).apply {
                type = mimes.first()
                if (multiple) {
                    putExtra(MediaStore.EXTRA_PICK_IMAGES_MAX, MediaStore.getPickImagesMaxLimit())
                }
            }
            if (pick.resolveActivity(packageManager) != null) return pick
        }

        // 其余（PDF、多类型、老系统、没相册选择器）：SAF 的 OPEN_DOCUMENT。
        // 它比 `createIntent()` 的 `ACTION_GET_CONTENT` 稳定，也不会撞上
        // Photo Picker 的 `ERR_UPLOAD_FILE_CHANGED`。
        return Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = mimes.firstOrNull() ?: "*/*"
            if (mimes.size > 1) putExtra(Intent.EXTRA_MIME_TYPES, mimes.toTypedArray())
            if (multiple) putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true)
        }
    }

    private fun errorView(text: String): TextView = TextView(this).apply {
        this.text = text
        setTextColor(Color.parseColor("#ff8080"))
        textSize = 13f
        setPadding(48, 160, 48, 48)
        setBackgroundColor(Color.BLACK)
    }

    @Deprecated("平台在 API 33 弃用了它；API 33+ 走下面的 OnBackInvokedCallback")
    @Suppress("DEPRECATION")
    override fun onBackPressed() {
        val w = web
        if (w != null && w.canGoBack()) {
            w.goBack()
        } else {
            super.onBackPressed()
        }
    }

    // -----------------------------------------------------------------------
    // 预测性返回（Android 13+ 的接口，14+ 才有跟手进度）
    //
    // 页面自己有覆盖层（抽屉/查看器/面板）时，它调 AgentHost.setBackHandled(true)，
    // 我们就注册系统返回回调：手势的进度会一路发到页面，由页面把那一层
    // 跟着手指挪 —— 也就是安卓原生的那种「跟手返回」。
    //
    // 没有覆盖层时不注册：让系统自己处理（退回桌面/上一个 App），
    // 它会画它自己的预测性动画。
    // -----------------------------------------------------------------------

    /** 页面的方法调用会跑到 JavaBridge 线程上，得自己回主线程 */
    inner class BackHost {
        @JavascriptInterface
        fun setBackHandled(handled: Boolean) {
            runOnUiThread {
                if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return@runOnUiThread
                if (handled) registerBackCallback() else unregisterBackCallback()
            }
        }

        /**
         * 头部的地球按钮（页面 → `AgentHost.openBrowser()`）。
         *
         * 没有它的话，页面那边是 `host()?.openBrowser?.()` —— 打了可选链，
         * 壳没实现就**静默什么都不发生**，按钮看着像坏的（就是这么踩的）。
         * 没开过页面时 `showBrowserPanel` 会弹一句「agent 还没打开过网页」。
         */
        @JavascriptInterface
        fun openBrowser() {
            runOnUiThread { showBrowserPanel() }
        }

        /**
         * agent 现在有没有开着的浏览器页面。头部那个地球按钮据此决定显不显示。
         *
         * 页面加载时同步问一次；之后的变化由壳**推**给它
         * （`window.__browserAvailable`，见 [pushBrowserAvailable]）。
         * 同步返回一个 boolean，不碰 UI，所以不用回主线程。
         */
        @JavascriptInterface
        fun hasBrowser(): Boolean = AgentApp.browser?.hasPage() == true
    }

    private fun registerBackCallback() {
        if (backCallback != null) return
        Log.i(TAG, "注册系统返回回调（页面里有覆盖层，SDK ${Build.VERSION.SDK_INT}）")
        val cb: Any = if (Build.VERSION.SDK_INT >= 34) {
            // 跟手进度在 OnBackAnimationCallback（API 34 才有的子接口）上，
            // 不是藏在 OnBackInvokedCallback 里
            object : android.window.OnBackAnimationCallback {
                override fun onBackStarted(backEvent: android.window.BackEvent) = sendBack("start", 0f)
                override fun onBackProgressed(backEvent: android.window.BackEvent) =
                    sendBack("progress", backEvent.progress)
                override fun onBackCancelled() = sendBack("cancel", 1f)
                override fun onBackInvoked() = sendBack("commit", 1f)
            }
        } else {
            // API 33：只有回调本身，没有进度。页面收到 commit 照样会关，
            // 只是没有跟手那一段
            object : android.window.OnBackInvokedCallback {
                override fun onBackInvoked() = sendBack("commit", 1f)
            }
        }
        @Suppress("NewApi")
        onBackInvokedDispatcher.registerOnBackInvokedCallback(
            android.window.OnBackInvokedDispatcher.PRIORITY_DEFAULT,
            cb as android.window.OnBackInvokedCallback,
        )
        backCallback = cb
    }

    private fun unregisterBackCallback() {
        val cb = backCallback ?: return
        backCallback = null
        Log.i(TAG, "注销系统返回回调（覆盖层收干净了）")
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        @Suppress("NewApi")
        onBackInvokedDispatcher.unregisterOnBackInvokedCallback(
            cb as android.window.OnBackInvokedCallback,
        )
    }

    /** 把手势进度交给页面（`web/src/lib/overlay.ts` 的 `__predictiveBack`） */
    private fun sendBack(phase: String, progress: Float) {
        if (phase == "commit") Log.i(TAG, "返回手势完成 → 页面")
        web?.evaluateJavascript("window.__predictiveBack && window.__predictiveBack('$phase', $progress)", null)
    }

    override fun onDestroy() {
        // 页面已经没了，别把一个永远等不到结果的文件选择请求留着
        fileCallback?.onReceiveValue(null)
        fileCallback = null
        unregisterBackCallback()
        // 看板：把 agent 那只 WebView 摘下来，别让它挂在一个马上要销毁的容器上。
        // **不销毁它** —— 它属于应用进程，回合可能还在跑（见 BrowserController）。
        panelTick.removeCallbacks(pollPanel)
        unregisterPanelBack()
        AgentApp.browser?.detach()
        // 浏览器属应用进程（比 Activity 活得久），回调别留着拽住这个 Activity
        AgentApp.browser?.onPagesChanged = null
        web?.destroy()
        web = null
        super.onDestroy()
    }

    // -----------------------------------------------------------------------

    private fun isDebuggable(): Boolean =
        (applicationInfo.flags and android.content.pm.ApplicationInfo.FLAG_DEBUGGABLE) != 0

    /**
     * 首次启动时要「所有文件访问权限」。
     *
     * 沙箱的 /workspace 挂在手机内部存储的 Agent 文件夹上 —— 用户在文件
     * 管理器里能直接看到 AI 的产出，卸载 App 也不会丢；/sdcard 是整个存储。
     * 没有这个权限就退回 App 私有目录。只问一次，返回 true 表示这次问了。
     */
    private fun maybeAskAllFilesAccess(): Boolean {
        if (AgentApp.hasAllFilesAccess(this)) return false
        val prefs = getSharedPreferences(PREFS, MODE_PRIVATE)
        if (prefs.getBoolean(KEY_ASKED_FILES, false)) return false
        prefs.edit().putBoolean(KEY_ASKED_FILES, true).apply()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val i = Intent(
                Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION,
                android.net.Uri.parse("package:$packageName"),
            )
            runCatching { startActivity(i) }
                .recoverCatching { startActivity(Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION)) }
                .onFailure { Log.w(TAG, "打不开所有文件访问设置页", it) }
        } else {
            requestPermissions(
                arrayOf(
                    Manifest.permission.READ_EXTERNAL_STORAGE,
                    Manifest.permission.WRITE_EXTERNAL_STORAGE,
                ),
                REQ_STORAGE,
            )
        }
        return true
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == REQ_STORAGE) AgentApp.syncSharedStorage(this)
    }

    /**
     * 首次启动时把用户送到无障碍设置页。
     *
     * 只问一次 —— 每次启动都跳转是很讨厌的行为。没开的话用户在
     * 「事件」页能看到 `设备已断开`，知道该去开。
     */
    private fun maybeOpenAccessibilitySettings() {
        val prefs = getSharedPreferences(PREFS, MODE_PRIVATE)
        if (prefs.getBoolean(KEY_ASKED_A11Y, false)) return

        val enabled = Settings.Secure.getString(
            contentResolver,
            Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES,
        ).orEmpty()

        if (!enabled.contains(packageName)) {
            prefs.edit().putBoolean(KEY_ASKED_A11Y, true).apply()
            runCatching {
                startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS))
            }.onFailure { Log.w(TAG, "打不开无障碍设置页", it) }
        }
    }
}
