package dev.vihv.agent

import android.annotation.SuppressLint
import android.content.Context
import android.os.SystemClock
import android.util.Log
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.ConcurrentHashMap

/**
 * 订阅核心的回合状态（SSE：`GET /api/turns/stream`），喂给 [LiveTurnStatus]。
 *
 * **手机自己的核心和「连着的那台电脑」是同一个实现** —— 两边都只是「HTTP + 一个 token」，
 * 只差 base URL：
 *
 * ```
 * http://127.0.0.1:<本地端口>    或    http://192.168.x.x:<端口>
 *
 * {"type":"turn","status":{…}}         → LiveTurnStatus.update（通知栏那一行）
 * {"type":"turn_active","active":…}    → 起/停前台服务
 * ```
 *
 * 谁在跑都从同一条流里来：本地（同进程）和「连电脑」都走它 —— 分成两套的话
 * 形状不一样、状态语义要维护两遍（见 docs/06）。
 *
 * 为什么不在 WebView 里做这件事：App 切到后台，WebView 的 JS 会被系统挂起，而通知栏
 * 恰恰是「App 不在前台」时才有用。
 *
 * 断线重连：2 秒一次，直到 [stop]。核心重启、手机刚从系统冻结里醒来、Wi-Fi 抖一下，
 * 都靠它自己接上；接上时服务端会先补一帧当前状态（`AppState::turn_snapshot`）。
 */
// lint 会报 「静态字段持有 Context」（StaticFieldLeak）。这里存的是 **Application** context
// （`sync()` 里取的 `applicationContext`），它本来就与进程同寿命 —— 不是泄漏，是唯一能
// 给「App 不在前台时」发通知的东西（拿 Activity 才会漏）。
@SuppressLint("StaticFieldLeak")
object TurnStream {

    private const val TAG = "TurnStream"
    private const val RETRY_MS = 2_000L

    /** 重连退避的上限。连不上（核心还没起、老核心没这个端点）时别每 2 秒刷一条日志。 */
    private const val RETRY_MAX_MS = 30_000L

    /**
     * 读超时。**不能设 0**：SEV 长连接靠服务端每 15 秒一条 keep-alive 注释心跳，
     * 设一个比它大的超时（而且不是无限等）才能发现「网断了但对端没关连接」。
     */
    private const val READ_TIMEOUT_MS = 45_000

    /** 一个订阅目标。同一个 base 只留一个。 */
    private class Sub(val base: String, val token: String) {
        @Volatile
        var stop = false

        /** 这个源上现在有没有回合在跑。前台服务的起停看**所有源的总和**。 */
        @Volatile
        var active = false

        /** 下一次重连等多久。连上过就退回 [RETRY_MS]，一直连不上就翻倍到 [RETRY_MAX_MS]。 */
        @Volatile
        var retry = RETRY_MS

        /** 上一次是**失败**收场（没读到过东西）。回到前台时据此立刻重试一次。 */
        @Volatile
        var failed = false

        /** 正连着的那条。退订时直接断开它，读线程才不会卡在 `readLine` 里再处理一帧旧状态 */
        @Volatile
        var conn: HttpURLConnection? = null
    }

    private val subs = ConcurrentHashMap<String, Sub>()

    /** App 级 Context：只用来发通知/起服务，不持有 Activity。 */
    @Volatile
    private var app: Context? = null

    /**
     * 把订阅对齐到「应该订哪些目标」：多退少补，幂等。
     *
     * 调用方每次把想要的清单整个算出来就行（连着/切回/回到前台都算一次），不用自己维护
     * 「上一次订了什么」那套状态。同一个 base 的 token 变了（重新配对）也会换掉重连。
     */
    fun sync(ctx: Context, targets: Map<String, String>) {
        app = ctx.applicationContext
        for (base in subs.keys.toList()) {
            if (base !in targets) stop(base)
        }
        for ((base, token) in targets) start(base, token)
    }

    private fun start(base: String, token: String) {
        if (base.isBlank() || token.isBlank()) return
        subs[base]?.let { cur ->
            // token 没变、而且连得好好的：什么都不用做。
            // token 变了（重新配对）要换；还在重试里（可能正等在 30 秒的退避上）也换 ——
            // 回到前台/切核心时重新算清单就是一个「现在再试一次」的机会，不然用户重启完
            // 核心还得干等退避。
            if (cur.token == token && !cur.failed) return
            stop(base)
        }
        val s = Sub(base, token)
        subs[base] = s
        Thread({ follow(s) }, "turn-stream").apply { isDaemon = true }.start()
        Log.i(TAG, "订阅回合状态: $base")
    }

    private fun stop(base: String) {
        val s = subs.remove(base) ?: return
        s.stop = true
        s.active = false
        // 断开在另一个线程上：disconnect 可能要碰网络，不能放在调用方（主线程）上
        s.conn?.let { c -> Thread { runCatching { c.disconnect() } }.start() }
        Log.i(TAG, "退订: $base")
        // 可能刚才是它在撑着前台服务
        refreshForeground()
    }

    /** 连着重试，直到 [Sub.stop]。 */
    private fun follow(s: Sub) {
        while (!s.stop) {
            s.failed = true
            try {
                read(s)
                if (!s.stop) Log.w(TAG, "订阅断了（服务端关了连接），重连: ${s.base}")
            } catch (t: Throwable) {
                if (!s.stop) {
                    Log.w(TAG, "订阅断了（${t.javaClass.simpleName}: ${t.message}），${s.retry}ms 后重连: ${s.base}")
                }
            }
            if (s.stop) break
            SystemClock.sleep(s.retry)
            s.retry = (s.retry * 2).coerceAtMost(RETRY_MAX_MS)
        }
    }

    /** 一条 SSE 连接，读到断为止。 */
    private fun read(s: Sub) {
        val conn = (URL("${s.base}/api/turns/stream").openConnection() as HttpURLConnection).apply {
            connectTimeout = 10_000
            readTimeout = READ_TIMEOUT_MS
            setRequestProperty("Cookie", "agent_token=${s.token}")
            setRequestProperty("Accept", "text/event-stream")
        }
        s.conn = conn
        try {
            val code = conn.responseCode
            // 502 是手机核心的隧道说「那台电脑哪条路都不通」（原因在隧道的日志里，见 tunnel.rs）
            if (code == HttpURLConnection.HTTP_BAD_GATEWAY) {
                throw IllegalStateException("连不上那台电脑（隧道回 502）")
            }
            if (code != HttpURLConnection.HTTP_OK) {
                throw IllegalStateException("回合状态流连不上（HTTP $code）")
            }
            val reader = conn.inputStream.bufferedReader()
            var sawLine = false
            while (!s.stop) {
                // 返回 null = 对端关了（或者读超时）—— 交给 follow() 重连
                val line = reader.readLine() ?: return
                if (!sawLine) {
                    // **真读到东西才算连上**，退避这时才归位：核心还没升级、没有这个端点时，
                    // 连接会「成功」然后立刻结束 —— 那时不能让退避归位，否则就是每 2 秒重试
                    // 一次、日志还刷屏
                    sawLine = true
                    s.retry = RETRY_MS
                    s.failed = false
                }
                // SSE：只认 `data:` 行；空行和 `:` 开头的 keep-alive 注释都不管
                if (line.startsWith("data:")) handle(s, line.removePrefix("data:").trim())
            }
        } finally {
            s.conn = null
            runCatching { conn.disconnect() }
        }
    }

    private fun handle(s: Sub, json: String) {
        // 已经退订了：这一帧是断开前最后读到的，不能再拿它改状态
        if (s.stop) return
        val o = runCatching { JSONObject(json) }.getOrNull() ?: return
        when (o.optString("type")) {
            "turn" -> o.optJSONObject("status")?.let { status ->
                app?.let { LiveTurnStatus.update(it, status.toString()) }
            }
            "turn_active" -> {
                s.active = o.optBoolean("active", false)
                refreshForeground()
            }
        }
    }

    /**
     * 起停前台服务：**只要有一个源在跑就得挂着**，全停下来才停。
     *
     * 两个源同时存在是正常的：连着电脑时手机自己的核心也还跑着（`just phone` 那种从
     * 电脑驱动手机的回合走的就是它），两边都可能各自开一个回合。
     */
    private fun refreshForeground() {
        val ctx = app ?: return
        if (subs.values.any { it.active }) {
            // 通知权限只能在 Activity 里问，而这是唯一一次「人就在看着」的时机
            MainActivity.current?.maybeAskNotificationPermission()
            AgentForegroundService.start(ctx)
        } else {
            LiveTurnStatus.turn(false)
            AgentForegroundService.stop(ctx)
        }
    }
}
