package dev.vihv.agent

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Build
import android.util.Log
import uniffi.agent_core.TurnHost

/**
 * 回合进行中挂着的前台服务。
 *
 * 为什么要有它：切到后台的回合没有保活，系统内存紧张时可以把进程杀掉 ——
 * 正在跑的 bash 死、浏览器那只 WebView（页面、登录态）也没了。会话历史能
 * 告诉你「做到哪一步」，但打了一半的东西就白打了（见 docs/06）。
 *
 * 和以前那个常驻版（`bdef4fc` 删掉的）的区别：**只在真有回合在跑时挂**。
 * 回合开始起服务、结束停服务，通知也随之出现/消失 —— 不再有那个「没有信息量
 * 但一直挂着的通知」。
 *
 * 两个平台限制决定了它的形态：
 *
 * - **Android 12+ 不许从后台起前台服务**（`ForegroundServiceStartNotAllowedException`）。
 *   回合通常是人从界面上发起的（那时 App 可见），由 [MainActivity] 顺手问一次
 *   通知权限；如果是脚本（agentctl）在后头起的回合，服务起不来就只降级，
 *   不报错 —— 反正它只是「尽量别被杀」。
 * - **前台服务不能没有通知**（没有「隐形前台服务」这回事）。这个通知至少
 *   说了实话：现在有东西在跑，点一下回到对话。
 */
class AgentForegroundService : Service() {

    companion object {
        private const val TAG = "AgentFgs"
        private const val CHANNEL_ID = "agent-turn"
        private const val NOTIF_ID = 1001

        /**
         * 起服务。**已经起了就是空操作** —— [TurnHost] 那边会话数一变就会调一次，
         * 重复调很正常。
         */
        fun start(ctx: Context) {
            runCatching {
                val i = Intent(ctx, AgentForegroundService::class.java)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    ctx.startForegroundService(i)
                } else {
                    ctx.startService(i)
                }
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

    override fun onBind(intent: Intent?) = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val nm = getSystemService(NotificationManager::class.java)
        if (nm.getNotificationChannel(CHANNEL_ID) == null) {
            nm.createNotificationChannel(
                NotificationChannel(
                    CHANNEL_ID,
                    getString(R.string.notif_channel_turn),
                    // LOW：不响、不弹，只在通知栏里待着 —— 它不是消息，是状态
                    NotificationManager.IMPORTANCE_LOW,
                ).apply { setShowBadge(false) },
            )
        }
        val notif = buildNotification()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            // API 34+：类型要在这里显式给（不能只靠 manifest）
            startForeground(
                NOTIF_ID,
                notif,
                android.content.pm.ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE,
            )
        } else {
            startForeground(NOTIF_ID, notif)
        }
        // 进程被杀之后不要重建：回合已经跟着没了，重建一个空服务没意义
        return START_NOT_STICKY
    }

    private fun buildNotification(): Notification {
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP),
            // IMMUTABLE：Android 12+ 必须显式指定
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )

        val b = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            Notification.Builder(this, CHANNEL_ID)
        } else {
            @Suppress("DEPRECATION")
            Notification.Builder(this)
        }
        return b.setSmallIcon(android.R.drawable.stat_notify_sync)
            .setContentTitle(getString(R.string.notif_turn_title))
            .setContentText(getString(R.string.notif_turn_text))
            .setContentIntent(open)
            .setOngoing(true)
            .build()
    }

    override fun onDestroy() {
        // 主动摘掉，别等系统超时（某些 ROM 上会留一条死通知）
        runCatching {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
                stopForeground(STOP_FOREGROUND_REMOVE)
            } else {
                @Suppress("DEPRECATION")
                stopForeground(true)
            }
        }
        super.onDestroy()
    }
}

/**
 * 把 Rust 那边「有回合在跑吗」翻译成前台服务的起/停。
 *
 * 从 agent 的 tokio 线程上被调用，所以这里只做两件很轻的事：起/停服务，
 * 顺带（可能）问一次通知权限。**不要在这里做重的活。**
 */
class TurnHostImpl(private val app: Context) : TurnHost {

    override fun turn(active: Boolean) {
        if (active) {
            // 通知权限只能在 Activity 里问，而这是唯一一次"人就在看着"的时机
            MainActivity.current?.maybeAskNotificationPermission()
            AgentForegroundService.start(app)
        } else {
            AgentForegroundService.stop(app)
        }
    }
}
