package dev.vihv.agent

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.util.Log
import android.view.View
import android.view.ViewGroup
import android.webkit.JavascriptInterface
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
        private const val REQ_NOTIF = 1
        private const val REQ_STORAGE = 2
    }

    private var web: WebView? = null
    private var fgsStarted = false
    /** 已注册的系统返回回调（API 33+；类型用 Any 存，低版本设备不会碰到这个类） */
    private var backCallback: Any? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        edgeToEdge()

        WebView.setWebContentsDebuggingEnabled(isDebuggable())

        val port = AgentApp.port
        if (port == 0) {
            setContentView(errorView("核心没起来。看 logcat 里 AgentApp 的报错。"))
        } else {
            setContentView(buildWebView(port))
        }

        askNotificationPermission()
        // 两个设置页只能一个一个来：先存储（沙箱干活的地方），下次启动再无障碍
        if (!maybeAskAllFilesAccess()) maybeOpenAccessibilitySettings()
    }

    /**
     * 前台服务在这里起，不在 Application.onCreate。
     *
     * Android 12+ 禁止从后台起前台服务；Activity 走到 onResume 才算
     * 真的进了前台。放这里就不会抛
     * `ForegroundServiceStartNotAllowedException`（实测踩过，
     * 而且那次直接崩了整个 App）。
     */
    override fun onResume() {
        super.onResume()
        if (!fgsStarted) {
            fgsStarted = true
            AgentForegroundService.start(this)
        }
        // 从设置页回来时权限可能刚打开：重挂沙箱的 /sdcard 和 /workspace
        AgentApp.syncSharedStorage(this)
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
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window.insetsController?.setSystemBarsAppearance(
                0,
                android.view.WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS,
            )
        }
    }

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
            override fun onConsoleMessage(m: android.webkit.ConsoleMessage?): Boolean {
                Log.d(TAG, "[web] ${m?.message()} @${m?.lineNumber()}")
                return true
            }
        }

        w.loadUrl("http://127.0.0.1:$port")
        web = w
        return w
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

    /** 页面的 setBackHandled 会跑到 JavaBridge 线程上，得自己回主线程 */
    inner class BackHost {
        @JavascriptInterface
        fun setBackHandled(handled: Boolean) {
            runOnUiThread {
                if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return@runOnUiThread
                if (handled) registerBackCallback() else unregisterBackCallback()
            }
        }
    }

    private fun registerBackCallback() {
        if (backCallback != null) return
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
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        @Suppress("NewApi")
        onBackInvokedDispatcher.unregisterOnBackInvokedCallback(
            cb as android.window.OnBackInvokedCallback,
        )
    }

    /** 把手势进度交给页面（`web/src/lib/overlay.ts` 的 `__predictiveBack`） */
    private fun sendBack(phase: String, progress: Float) {
        web?.evaluateJavascript("window.__predictiveBack && window.__predictiveBack('$phase', $progress)", null)
    }

    override fun onDestroy() {
        unregisterBackCallback()
        web?.destroy()
        web = null
        super.onDestroy()
    }

    // -----------------------------------------------------------------------

    private fun isDebuggable(): Boolean =
        (applicationInfo.flags and android.content.pm.ApplicationInfo.FLAG_DEBUGGABLE) != 0

    private fun askNotificationPermission() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) ==
            PackageManager.PERMISSION_GRANTED
        ) {
            return
        }
        requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), REQ_NOTIF)
    }

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
