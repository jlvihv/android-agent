package dev.vihv.agent

import android.net.Uri
import android.util.Log
import uniffi.agent_core.MachineInfo

/**
 * 「现在连的是哪台电脑」—— 壳这边只记这一件事。
 *
 * 配对、记账（配对过哪些电脑、各自的凭据和证书指纹、中继）、连接、**选路**（局域网还是
 * 中继）全在核心里（`core/src/machines.rs` + `tunnel.rs`），连哪台都是
 * `connectMachine(id)` 拿到一个地址，WebView 加载它就行 —— 在家走局域网、出门走中继，
 * 由核心在每条连接上自己决定，壳里没有分叉。
 *
 * 页面上那个「选择机器」面板也**不经过壳**了（见 docs/12）：面板问的是核心的
 * `GET /api/machines`（经隧道时隧道把那份请求转回本机核心），换机器时页面/核心把要去的
 * 地址交给壳 —— [navigate]。壳只做两件事：**认不认这个地址**（只认核心认可的那两个：
 * 本机的、和现在这条隧道的），以及换过去之后的记账（WebView 的历史要清、回合状态订阅要
 * 跟着换，见 `MainActivity.switchTo` / `syncTurnStream`）。
 *
 * 为什么要「认地址」：`AgentHost` 每个 frame 都看得见，包括查看器里 AI 写的网页 ——
 * 能传任意地址的话，一句 JS 就能把这个界面带到别人的服务器上（见 docs/11）。
 *
 * 「上次连的是哪台」记在**核心的库里**（`meta` 表的 `current_machine`），不在壳里：
 * 手机和电脑两边都是同一份实现，电脑上重启核心后浏览器那个标签还能接着用。
 */
class Remote {

    companion object {
        private const val TAG = "Remote"
    }

    /** 正连着的那台：实例 id + 这条隧道的地址（端口和钥匙每次连接都不一样，不落盘） */
    data class Conn(val id: String, val url: String) {
        private val u: Uri get() = Uri.parse(url)

        /** `http://localhost:<端口>`（回合状态订阅用） */
        val base: String get() = "http://${u.host}:${u.port}"
        val token: String? get() = u.getQueryParameter("t")
        val host: String? get() = u.host
        val port: Int get() = u.port
    }

    var conn: Conn? = null
        private set

    /** 配对过的电脑（核心给的，不带凭据）—— 「它不认我们的凭据了」时要说得出名字 */
    fun machines(): List<MachineInfo> = runCatching { AgentApp.core.machines() }.getOrDefault(emptyList())

    fun name(id: String): String =
        machines().firstOrNull { it.id == id }?.name?.takeIf { it.isNotBlank() } ?: "电脑"

    /**
     * 启动时：上次连着电脑就重新连上（是哪台记在核心的库里，隧道端口尽量复用上次那个）。
     * 返回 WebView 要加载的地址；没连过、或者那台已经不在了就是 null（回本机）。
     */
    fun resume(): String? {
        val url = runCatching { AgentApp.core.resumeMachine() }.getOrNull() ?: return null
        conn = urlToConn(url)
        return url
    }

    /**
     * 换到某个地址（页面让壳做的）。
     *
     * **只认核心认可的地方**：本机核心的地址，或现在这条隧道的地址（都从核心问）。
     * 比的是 `host:port` —— 页面那份地址里只有隧道钥匙（`/?k=…`），壳手里这份还带
     * 那台电脑的凭据（`TurnStream` 订阅回合状态要它），同一个端口就是同一个地方。
     * 认了就把「现在连的是哪台」记上，返回 true（调用方接着清历史、订阅回合状态）。
     */
    fun navigate(url: String, localUrl: String): Boolean {
        val place = place(url)
        if (place == place(localUrl)) {
            conn = null
            Log.i(TAG, "回到手机自己的核心")
            return true
        }
        val current = runCatching { AgentApp.core.currentMachineUrl() }.getOrNull()
        if (current == null || place != place(current)) {
            Log.w(TAG, "不认这个地址（只认核心给的那两个）：$url")
            return false
        }
        conn = urlToConn(current)
        Log.i(TAG, "连上「${conn?.let { name(it.id) }}」")
        return true
    }

    /** 回到手机自己的核心。 */
    fun useLocal() {
        runCatching { AgentApp.core.disconnectMachine() }
        conn = null
    }

    /** 它不认我们的凭据了（换过库）：忘掉，下次得重新配对 */
    fun forget(id: String) {
        runCatching { AgentApp.core.forgetMachine(id) }
        if (conn?.id == id) conn = null
    }

    /** 把地址拆成「哪台 + 那条隧道」。id 从核心问 —— 地址里只有隧道钥匙和凭据。 */
    private fun urlToConn(url: String): Conn {
        val id = runCatching { AgentApp.core.currentMachineId() }.getOrNull().orEmpty()
        return Conn(id, url)
    }

    /** 地址指的是哪个地方：`host:port`（不带凭据、不带路径）。 */
    private fun place(url: String): String {
        val u = Uri.parse(url)
        return "${u.host}:${u.port}"
    }
}
