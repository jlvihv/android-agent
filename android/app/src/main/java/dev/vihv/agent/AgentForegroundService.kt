package dev.vihv.agent

import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Log

/**
 * 回合进行中挂着的前台服务 —— 通知栏上那条**实时更新**的通知就是它挂的。
 *
 * 为什么要有它：切到后台的回合没有保活，系统内存紧张时可以把进程杀掉 ——
 * 正在跑的 bash 死、浏览器那只 WebView（页面、登录态）也没了。会话历史能
 * 告诉你「做到哪一步」，但打了一半的东西就白打了（见 docs/06）。
 *
 * 和以前那个常驻版（`bdef4fc` 删掉的）的区别：**只在真有回合在跑时挂**。
 * 回合开始起服务、结束停服务，通知随之出现/消失 —— 不再有那个「没有信息量
 * 但一直挂着的通知」。
 *
 * 这个类只管**活**（起、停、什么时候把通知改一遍），通知长什么样全在
 * [AgentNotifications] 里；内容从 [LiveTurnStatus]（唯一事实来源）读，而它是
 * Rust 的 agent loop 推过来的（`/api/turns/stream`，见 [TurnStream]）。形状照抄 OpenMinis 那边：
 * 前台服务订阅 tracker、每变一次就把通知就地改一遍 —— 我们订阅的是同一个东西，
 * 只是它的事实来源在 Rust 里。
 *
 * 两个平台限制决定了它的形态：
 *
 * - **Android 12+ 不许从后台起前台服务**（`ForegroundServiceStartNotAllowedException`）。
 *   回合通常是人从界面上发起的（那时 App 可见），由 [MainActivity] 顺手问一次
 *   通知权限；如果是脚本（agentctl）在后头起的回合，服务起不来就只降级，
 *   不报错 —— 反正它只是「尽量别被杀」。
 * - **前台服务不能没有通知**（没有「隐形前台服务」这回事）。这个通知至少
 *   说了实话：现在在干什么，点一下回到对话。
 *
 * **和 OpenMinis 的一处刻意不同**：App 在前台（人正看着对话）时，他们那条进行中
 * 的通知是**收起来**的（眼前已有的东西不必再通知一遍）。我们不收 —— 这里的前台
 * 服务同时还承担保活（docs/06 实测：离开焦点 60~75 秒后整个进程会被 freezer 冻住），
 * 收起来意味着每切一次前后台都要重新 promote 一次，而 OpenMinis 那边前台时本来
 * 就不需要保活，收起来的代价只有一点点。宁可多一条安静的状态行，也不要一个可能
 * 收不回来的服务。
 */
class AgentForegroundService : Service() {

    companion object {
        private const val TAG = "AgentFgs"

        /**
         * 起服务。**已经起了就是空操作** —— [TurnStream] 那边回合起/停时会调，
         * 重复调很正常。
         */
        fun start(ctx: Context) {
            runCatching {
                val i = Intent(ctx, AgentForegroundService::class.java)
                ctx.startForegroundService(i)
            }.onFailure {
                // 起不来只意味着"可能被系统回收"，不该让调用方崩掉
                Log.w(TAG, "前台服务起不来（不影响回合本身）: ${it.message}")
            }
        }

        fun stop(ctx: Context) {
            runCatching { ctx.stopService(Intent(ctx, AgentForegroundService::class.java)) }
                .onFailure { Log.w(TAG, "停前台服务失败: ${it.message}") }
        }
    }

    /** 主线程的把手：状态是从 [TurnStream] 的读线程推过来的，碰通知要回到主线程。 */
    private val main = Handler(Looper.getMainLooper())
    private val redraw = Runnable { render() }

    /** 状态订阅的退订函数。跟着服务的生死 —— 不这么绑，换个服务实例就漏一个订阅。 */
    private var unsubscribe: (() -> Unit)? = null

    override fun onBind(intent: Intent?) = null

    override fun onCreate() {
        super.onCreate()
        AgentNotifications.ensureChannels(this)
        // 状态一变就重画。唯一的触发源就是 Rust 推过来的新状态。
        unsubscribe = LiveTurnStatus.listen { scheduleRedraw() }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // 立刻画一遍：`startForegroundService` 之后 5 秒内必须 `startForeground`，
        // 否则系统直接杀进程（`ForegroundServiceDidNotStartInTimeException`）。
        // 所以这里同步做，不排到消息队列上去。
        render()
        // 进程被杀之后不要重建：回合已经跟着没了，重建一个空服务没意义
        return START_NOT_STICKY
    }

    /**
     * 重画排到主线程，并且**合并**重复请求。
     *
     * 流式正文半秒一次、工具切换更快，但一帧还没画完又来三帧没有意义 —— 后面那几帧
     * 读到的是同一份最新状态，白刷三次通知（每次还是一次 binder 往返）。
     */
    private fun scheduleRedraw() {
        main.removeCallbacks(redraw)
        main.post(redraw)
    }

    /**
     * 按当前状态画一遍。**每一次都是 `startForeground` 而不是
     * `NotificationManager.notify()`**。
     *
     * 两者都能把同 id 的通知换内容，但语义不一样：`notify()` 会把通知重新登记成一条
     * **普通**通知（记录上的 `FLAG_FOREGROUND_SERVICE` 随之消失），于是服务结束时系统
     * 不再顺手收它 —— 实测就是这样：回合结束后通知栏里留着一条「已完成」。
     * 而 `startForeground` 每次都会把「前台服务的通知」这个身份重新钉上（Android 文档里
     * 更新前台服务通知就是这个用法），服务一死通知就跟着走。代价一样是一次 binder 往返，
     * 那就用对的那个。
     */
    private fun render() {
        val view = LiveTurnStatus.view()
        val notification = try {
            AgentNotifications.ongoing(this, view)
        } catch (t: Throwable) {
            Log.w(TAG, "通知画不出来（不影响回合）: ${t.message}")
            return
        }
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                // API 34+：类型要在这里显式给（不能只靠 manifest）
                startForeground(
                    AgentNotifications.ID_LIVE,
                    notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE,
                )
            } else {
                startForeground(AgentNotifications.ID_LIVE, notification)
            }
        } catch (t: Throwable) {
            // 挂不上去（Android 12+ 的后台限制等）：服务没用，但**回合本身照跑**，
            // 不能把进程带崩
            Log.w(TAG, "挂前台服务失败（${t.javaClass.simpleName}: ${t.message}）")
            stopSelf()
        }
    }

    override fun onDestroy() {
        unsubscribe?.invoke()
        unsubscribe = null
        main.removeCallbacks(redraw)
        // 主动摘掉通知，别等系统超时（某些 ROM 上会留一条死通知）。
        //
        // **两道都要做**，这不是保险起见：实测（模拟器 API 35）只用
        // `stopForeground(STOP_FOREGROUND_REMOVE)` 时，回合结束后那条通知会**留在
        // 通知栏里**——记录上的 `FLAG_FOREGROUND_SERVICE` 不知何时已经被抹掉了
        // （前台服务的通知一旦不是「前台服务通知」，服务死掉时系统就不会顺手收它）。
        // 所以除了摘掉前台身份，再按 id 明确撤一次。
        runCatching { stopForeground(STOP_FOREGROUND_REMOVE) }
        runCatching {
            getSystemService(NotificationManager::class.java)?.cancel(AgentNotifications.ID_LIVE)
        }
        super.onDestroy()
    }
}
