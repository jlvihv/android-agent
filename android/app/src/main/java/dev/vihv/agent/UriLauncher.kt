package dev.vihv.agent

import android.app.ActivityManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.util.Log

/**
 * 把 URI 交给系统：浏览器、对应的 App，或 `intent://` 指定的任意组件。
 *
 * 两个入口都用它：
 *  - **用户**在聊天里点链接（`MainActivity` 的 `shouldOverrideUrlLoading` /
 *    `onCreateWindow`）；
 *  - **agent** 自己要求打开（沙箱里的 `agent-open` / `$BROWSER` / `xdg-open`
 *    → `agent open` → `POST /api/command` 的 `open` 指令）。开着无障碍时走
 *    `DeviceHostImpl`（用无障碍服务的 context，后台也能拉起页面）；没开时走
 *    `BrowserController` 的 `__open_uri`（[openFromApp]，只在 App 在前台时行）。
 *
 * 两边都**不做 scheme 白名单**：agent 操作的是用户自己的手机，跟 `agent tap` / `agent key`
 * 那些设备操作一个立场 —— 调用直接执行，没有审批环节。剩下的只是"系统能不能接"。
 */
object UriLauncher {
    private const val TAG = "AgentUriLauncher"

    /**
     * 打开 [uri]，成功返回一句给 agent / 日志看的话；失败抛异常（消息给人看）。
     *
     * `intent://…` 必须用 `Intent.parseUri` —— `#Intent;` 后面的 action /
     * category / extras 只有它能还原，裸 `ACTION_VIEW` 开不了。没有 App 接时按
     * 约定读 `S.browser_fallback_url` 再开一次，这是 `intent://` 链接的标准兜底
     * （网页上生成的 intent 链接都带着它）。
     */
    fun open(context: Context, uri: Uri): String {
        val scheme = uri.scheme?.lowercase()
        if (scheme == "intent" || scheme == "android-app") return openParsedIntent(context, uri)
        startView(context, Intent(Intent.ACTION_VIEW, uri))
        return "opened $uri"
    }

    /**
     * 没有无障碍服务时的入口：用 Application 的 context 打开。
     *
     * Android 10+ 不许普通 App 从后台起 Activity —— 而且被拦时**不抛异常**，
     * 只是什么都不发生。所以先自己看一眼在不在前台，不在就明说，别让 agent
     * 以为已经打开了。（前台服务不算：它不豁免这条限制。）
     */
    fun openFromApp(context: Context, uri: Uri): String {
        val me = ActivityManager.RunningAppProcessInfo()
        ActivityManager.getMyMemoryState(me)
        val visible = me.importance == ActivityManager.RunningAppProcessInfo.IMPORTANCE_FOREGROUND ||
            me.importance == ActivityManager.RunningAppProcessInfo.IMPORTANCE_VISIBLE
        check(visible) {
            "App 在后台、无障碍服务也没开，系统不允许从后台打开页面：开启无障碍服务，或请用户切回 App 后再试"
        }
        return open(context, uri)
    }

    private fun openParsedIntent(context: Context, uri: Uri): String {
        val intent = try {
            Intent.parseUri(uri.toString(), Intent.URI_INTENT_SCHEME)
        } catch (t: Throwable) {
            throw IllegalArgumentException("intent:// 解析不了: ${t.message}")
        }
        // 别按页面指定的方式叠一层新文档栈（其余 action/category/extras 原样保留）
        intent.removeFlags(Intent.FLAG_ACTIVITY_NEW_DOCUMENT)
        val fallback = intent.getStringExtra("browser_fallback_url")
        intent.removeExtra("browser_fallback_url")

        try {
            startView(context, intent)
            return "opened $uri"
        } catch (t: Throwable) {
            if (fallback.isNullOrEmpty()) throw t
            Log.i(TAG, "没有 App 接 $uri，退回浏览器: $fallback")
            startView(context, Intent(Intent.ACTION_VIEW, Uri.parse(fallback)))
            return "opened fallback $fallback"
        }
    }

    private fun startView(context: Context, intent: Intent) {
        // 两个入口都可能是"没有 Activity 在前台"的场景（agent 在后台跑），
        // 不给 NEW_TASK 会直接 ActivityNotFoundException
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        context.startActivity(intent)
    }
}
