package dev.vihv.agent

import android.content.Context
import android.os.SystemClock
import android.util.Log
import org.json.JSONObject
import java.util.concurrent.CopyOnWriteArrayList

/**
 * 阶段。Rust 那边（`status::Phase`）传过来的是字符串，这里**一次收敛成枚举** ——
 * 免得界面代码里到处比字符串（改了好几轮 phase，字符串漏一个就静默落到兜底文案上）。
 *
 * [OTHER] 是留给**比壳新的核心**的：核心加了第五个阶段时，旧壳不会崩，只是说句兜底的话。
 */
enum class Phase {
    THINKING,
    STREAMING,
    TOOL,
    DONE,
    OTHER;

    companion object {
        fun of(raw: String) = when (raw) {
            "thinking" -> THINKING
            "streaming" -> STREAMING
            "tool" -> TOOL
            "done" -> DONE
            else -> OTHER
        }
    }
}

/**
 * 一个会话此刻在干什么 —— Rust 推过来的那份快照（见 `core/src/status.rs`）。
 */
data class TurnStatus(
    val sid: String,
    /** 会话标题（侧边栏那一行）。空串 = 还没起标题，通知栏用兜底文案 */
    val title: String,
    val phase: Phase,
    /** 正在跑的工具名（`bash` / `read` / `edit`），没有就是 null */
    val tool: String?,
    /** 一行细节：命令、路径、或正在写的那一行 */
    val detail: String?,
    /** 本回合开始的时刻（**wall clock 毫秒**，Rust 那边已经换算好了） */
    val startedAtMs: Long,
    /** 结束时刻，只有结束那一条才有 */
    val finishedAtMs: Long?,
    /** 还在跑吗 */
    val active: Boolean,
    /** 结束时的结果：正常跑完 = true，出错 = false */
    val ok: Boolean,
    /** 结束是不是因为用户按了停止 */
    val stopped: Boolean,
    /**
     * 收到它的时刻（**单调时钟** `elapsedRealtime`）。两件事都用它：
     * 挑「最近动过的那个会话」排序，以及判断「刚完事的那一条还新不新鲜」。
     * 单调而不是墙钟：只做先后比较，不涉及时区/校时。
     */
    val at: Long,
)

/**
 * **实时状态的唯一事实来源。**
 *
 * 形状照抄 OpenMinis 的 `SessionActivityTracker`：一个对象存着「现在谁在跑、跑到哪
 * 一步了」，前台服务订阅它、每次变化就把通知就地改一遍。区别只在事实从哪儿来 ——
 * 他们的状态在 Kotlin 里（agent loop 也在 Kotlin 里），我们的状态在 Rust 里
 * （agent loop 在 Rust 里），所以要有人把核心推过来的 JSON 写进来 —— 那个人是 [TurnStream]
 * （本地核心和「连着的那台电脑」订阅的是同一条 SSE，见它）：
 *
 * ```
 * Rust status::Reporter ──Event::Turn──▶ TurnStream ──▶ LiveTurnStatus.update()
 *                                                     │ 变化
 *                        AgentForegroundService ◀─────┘（订阅者，重画进行中的通知）
 *                        AgentNotifications.showDone ◀─（完事那一条，独立于服务）
 * ```
 *
 * 两条消费者刻意分开：**进行中**那条必须挂在前台服务上（服务没了它就得没），
 * **完成**那条要在服务停掉之后还能留着（人回来看手机才知道干完了）。
 */
object LiveTurnStatus {

    private const val TAG = "LiveTurnStatus"

    /** 在跑的会话（外加刚跑完的那一条）→ 它此刻的状态。整份换，不在原地改。 */
    @Volatile
    private var bySession: Map<String, TurnStatus> = emptyMap()

    /** 状态变了要重画通知的人（前台服务一个，随服务生死）。 */
    private val listeners = CopyOnWriteArrayList<() -> Unit>()

    /**
     * 「刚完事」那一条还能拿来当最后一帧看多久。
     *
     * 进行中那条通知的命是前台服务的：回合结束的一瞬间它就要被拆掉。那一刻画什么？
     * 如果状态已经清空了，服务会画成兜底文案（「正在处理…」）—— 用户可能看到一条
     * 「刚刚都干完了，却写成正在处理」的闪帧。所以结束那一条**留着**，让最后一帧是
     * 「已完成」。但要有个期限：太旧的（上个会话、十分钟前）不该再冒充「现在」。
     */
    private const val DONE_FRESH_MS = 60_000L

    /** 通知栏要画的那一份：主会话 + 有几个会话在跑。 */
    data class View(val status: TurnStatus, val activeCount: Int)

    /**
     * Rust 那边的一份状态（`status::Snapshot` 的 JSON）。从 agent 的 tokio 线程调用，
     * 所以这里只做「改一份内存里的状态 + 让订阅者去重画」。
     */
    fun update(app: Context, json: String) {
        val s = try {
            parse(json)
        } catch (t: Throwable) {
            // 状态是给通知栏看的，不是回合本身 —— 解析不了就当这一帧没有，
            // 绝不能让它把 agent 的活儿带崩
            Log.w(TAG, "状态解析失败: ${t.message}（$json）")
            return
        }
        bySession = bySession + (s.sid to s)
        for (l in listeners) {
            runCatching { l() }.onFailure { Log.w(TAG, "刷新通知失败: ${it.message}") }
        }
        // 完事那一条：用户没在看的时候提醒一声（正看着就不用 —— 结果就在眼前）。
        // 放在这里而不是前台服务里：服务在这之后马上就被停掉了（`turn(false)`），
        // 而这条通知得留下来等人回来看。
        if (!s.active) AgentNotifications.showDone(app, s)
    }

    /**
     * [TurnStream] 收到 `turn_active=false` 时调：还有回合在跑吗（前台服务的起/停跟它走）。
     *
     * 全部停下来之后清理一遍地图：**刚完事那几条留着**（它们可能正被画成最后一帧，
     * 见 [view]），更早的那些扔掉 —— 它们该做的都做完了（完成通知已经发出去），
     * 留着只会让下次「谁在跑 / 有几个」算错。
     */
    fun turn(active: Boolean) {
        if (active) return
        val now = SystemClock.elapsedRealtime()
        val kept = bySession.filterValues { it.active || now - it.at <= DONE_FRESH_MS }
        if (kept.size != bySession.size) bySession = kept
    }

    /** 订阅状态变化。返回退订用的函数（服务必须在 `onDestroy` 里调它）。 */
    fun listen(l: () -> Unit): () -> Unit {
        listeners += l
        return { listeners -= l }
    }

    /** 通知栏现在该画什么。什么都没有、又没有「刚完事」的，就是 `null`（服务用兜底文案）。 */
    fun view(): View? {
        val all = bySession.values
        val live = all.filter { it.active }
        // 有在跑的：画最近动过的那个（多会话时只画一个，另外几个用文字带上）
        if (live.isNotEmpty()) return View(live.maxByOrNull { it.at }!!, live.size)
        // 没有在跑的：只认「刚完事」的那一条 —— 这一帧正是「刚跑完」
        val fresh = SystemClock.elapsedRealtime() - DONE_FRESH_MS
        return all
            .filter { !it.active && it.at >= fresh }
            .maxByOrNull { it.at }
            ?.let { View(it, 0) }
    }

    private fun parse(json: String): TurnStatus {
        val o = JSONObject(json)
        return TurnStatus(
            sid = o.getString("sid"),
            title = o.optString("title"),
            phase = Phase.of(o.optString("phase")),
            tool = o.optString("tool").takeIf { it.isNotEmpty() },
            detail = o.optString("detail").takeIf { it.isNotEmpty() },
            startedAtMs = o.optLong("started_at_ms"),
            finishedAtMs = if (o.has("finished_at_ms")) o.optLong("finished_at_ms") else null,
            active = o.optBoolean("active", true),
            ok = o.optBoolean("ok", true),
            stopped = o.optBoolean("stopped", false),
            at = SystemClock.elapsedRealtime(),
        )
    }
}
