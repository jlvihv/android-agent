package dev.vihv.agent

import android.content.Context
import android.net.Uri
import android.util.Log
import uniffi.agent_core.MachineInfo

/**
 * 「现在连的是哪台电脑」—— 壳这边只记这一件事。
 *
 * 配对、记账（配对过哪些电脑、各自的凭据和证书指纹、中继）、连接、**选路**（局域网还是
 * 中继）全在核心里（`core/src/machines.rs` + `tunnel.rs`）。连哪台都是同一个动作：
 * `connectMachine(id)` 拿到 `http://localhost:<端口>/auth?t=…`，WebView 加载它就行 ——
 * 在家走局域网、出门走中继，由核心在每条连接上自己决定，壳里没有分叉。
 *
 * 记在 SharedPreferences 里的只有「上次连的是哪台」（下次启动接着连）。
 */
class Remote(private val ctx: Context) {

    companion object {
        private const val TAG = "Remote"
        private const val PREFS = "agent"
        /** 上次连的那台电脑的实例 id；没有 = 用手机自己的核心 */
        private const val KEY_CURRENT = "remote_core_id"
    }

    /** 正连着的那台：实例 id + 这次隧道的地址（端口每次都不一样，不落盘） */
    data class Conn(val id: String, val url: String) {
        private val u: Uri get() = Uri.parse(url)

        /** `http://localhost:<端口>`（回合状态订阅、判断「是不是它的页面」用） */
        val base: String get() = "http://${u.host}:${u.port}"
        val token: String? get() = u.getQueryParameter("t")
        val host: String? get() = u.host
        val port: Int get() = u.port
    }

    var conn: Conn? = null
        private set

    private fun prefs() = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /** 配对过的电脑（核心给的，不带凭据） */
    fun machines(): List<MachineInfo> = runCatching { AgentApp.core.machines() }.getOrDefault(emptyList())

    fun name(id: String): String = machines().firstOrNull { it.id == id }?.name?.takeIf { it.isNotBlank() } ?: "电脑"

    fun known(id: String): Boolean = machines().any { it.id == id }

    /** 启动时：上次连着电脑就重新连上（隧道端口每次不同）。连不上（被忘掉了）就回本机。 */
    fun resume(): String? {
        val id = prefs().getString(KEY_CURRENT, null) ?: return null
        return connect(id) ?: run {
            prefs().edit().remove(KEY_CURRENT).apply()
            null
        }
    }

    /** 连上这台，返回 WebView 要加载的地址；失败是 null（原因在 logcat） */
    fun connect(id: String): String? {
        val url = runCatching { AgentApp.core.connectMachine(id) }
            .onFailure { Log.w(TAG, "连「${name(id)}」失败：${it.message}") }
            .getOrNull() ?: return null
        conn = Conn(id, url)
        prefs().edit().putString(KEY_CURRENT, id).apply()
        Log.i(TAG, "连上「${name(id)}」")
        return url
    }

    /** 回到手机自己的核心 */
    fun disconnect() {
        runCatching { AgentApp.core.disconnectMachine() }
        conn = null
        prefs().edit().remove(KEY_CURRENT).apply()
    }

    /** 它不认我们的凭据了（换过库）：忘掉，下次得重新配对 */
    fun forget(id: String) {
        runCatching { AgentApp.core.forgetMachine(id) }
        if (conn?.id == id) disconnect()
    }
}
