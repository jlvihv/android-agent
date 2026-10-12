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

        // 两个设置页只能一个一个来：先存储（沙箱干活的地方），下次启动再无障碍
        if (!maybeAskAllFilesAccess()) maybeOpenAccessibilitySettings()
    }

    override fun onResume() {
        super.onResume()
        current = this
        // 从设置页回来时权限可能刚打开：重挂沙箱的 /sdcard 和 /workspace
        AgentApp.syncSharedStorage(this)
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
