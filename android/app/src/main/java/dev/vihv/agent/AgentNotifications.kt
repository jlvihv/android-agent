package dev.vihv.agent

import android.annotation.SuppressLint
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.util.Log

/**
 * 通知栏那两条通知长什么样、什么时候发。
 *
 * 形状照抄 OpenMinis 的 `AgentForegroundService` + `BackgroundTaskNotifier`：
 *
 * 1. **进行中**（[ID_LIVE]，挂在前台服务上）：内容随回合实时变。两行 ——
 *    第一行是哪个会话，第二行是它此刻在干什么（跑哪条命令 / 在想 / 在写什么）。
 *    没有进度条、没有秒表、没有「停止」按钮：agent 报不出诚实的百分比，而一个
 *    停在半路的计时器比没有计时器更糟（OpenMinis 那边就踩过：任务结束后计时器
 *    还在跳）。
 * 2. **完成**（[ID_DONE]）：跑完那一刻发一条，能点掉（[showDone]）。**总是发**，不只在
 *    切后台的时候 —— 进行中那条会跟着服务一起消失，而「消失了」等于什么都没说。回到 App
 *    看过才清（[clearDone]）；用户正看着 App 时不响，不看着时响。
 *
 * 两件事分开是有原因的：进行中那条的命跟着前台服务（服务停 = 通知消失），
 * 而完成那条要活得更久（人放下手机、过一会儿回来看，才知道干完了）。
 */
object AgentNotifications {

    private const val TAG = "AgentNotif"

    /**
     * 进行中那条的通道 / id。**沿用老值**：通道是用户设置过的（重要性、是否静音），
     * 换 id 等于换一条新通道、把用户的设置扔掉。
     *
     * 除了进行中那条，[showDone] 在「用户正看着 App」时也走这条通道：那要的是一条
     * **不响**的完成通知（id 仍用 [ID_DONE]，只是换条渠道走）。
     */
    const val CHANNEL_LIVE = "agent-turn"
    const val ID_LIVE = 1001

    /** 完成那条。**总是**发（人在不看着时响），单独一条通道：它可以响、可以点掉，
     *  而进行中那条不行（LOW、常驻）。 */
    const val CHANNEL_DONE = "agent-done"

    /**
     * 完成那条的 id。和进行中那条分开：进行中那条的命是前台服务的（服务停就销），
     * 这条要留在通知栏里等人回来看。
     */
    const val ID_DONE = 1002

    /**
     * Android 16 请求「提升为 Live Update」用的 extras key。
     *
     * 不是一个笔误：公开 SDK 里 `Notification.Builder.setRequestPromotedOngoing()`
     * 还没导出（@FlaggedApi），而系统判定 `hasPromotableCharacteristics()` 时读的
     * 就是这个布尔（`android.requestPromotedOngoing`）。`FLAG_PROMOTED_ONGOING`
     * 是系统**决定提升之后**打上的标志，不是我们的请求 —— 这两件事不是一回事。
     * （做法和取值都来自 OpenMinis，他们在 Pixel 上验过。）
     */
    private const val EXTRA_REQUEST_PROMOTED_ONGOING = "android.requestPromotedOngoing"

    /** 状态栏芯片那一行只有几个字宽（见 [chipText]）。 */
    private const val CHIP_MAX_UNITS = 9

    /** 上一次画在哪个面上（true = 提升成芯片，false = 普通通知）；只在切换时记日志 */
    @Volatile
    private var lastSurface: Boolean? = null

    /**
     * 记一条「这次画在哪个面上」。
     *
     * 真机上「为什么没有 Live Updates 芯片」只有两种可能：能力探针说不行（[LiveUpdates]
     * 会记一条带 SDK 和 false 的日志），或者我们根本没走那条分支 —— 这两行日志把
     * 二选一变成确定的。（第三道：走了但系统没提，那会让 [promoted] 里的警告带出每一项前提。）
     */
    private fun noteSurface(promoted: Boolean) {
        if (lastSurface == promoted) return
        lastSurface = promoted
        Log.i(
            TAG,
            if (promoted) "进行中的通知走 Live Updates（请求提升为常驻芯片）"
            else "进行中的通知走普通通知（Live Updates 不可用或已被关掉）",
        )
    }

    fun ensureChannels(ctx: Context) {
        val nm = ctx.getSystemService(NotificationManager::class.java) ?: return
        if (nm.getNotificationChannel(CHANNEL_LIVE) == null) {
            nm.createNotificationChannel(
                NotificationChannel(
                    CHANNEL_LIVE,
                    ctx.getString(R.string.notif_channel_turn),
                    // LOW：不响、不弹，只在通知栏里待着 —— 它不是消息，是状态
                    NotificationManager.IMPORTANCE_LOW,
                ).apply {
                    description = ctx.getString(R.string.notif_channel_turn_desc)
                    setShowBadge(false)
                },
            )
        }
        if (nm.getNotificationChannel(CHANNEL_DONE) == null) {
            nm.createNotificationChannel(
                NotificationChannel(
                    CHANNEL_DONE,
                    ctx.getString(R.string.notif_channel_done),
                    NotificationManager.IMPORTANCE_DEFAULT,
                ).apply {
                    description = ctx.getString(R.string.notif_channel_done_desc)
                    setShowBadge(true)
                },
            )
        }
    }

    /**
     * 进行中那一条。`view == null` = 回合刚开始、第一份状态还没推过来（Rust 那边
     * `begin_turn` 和 agent loop 起步之间有一瞬间），那就说一句兜底的实话。
     */
    fun ongoing(ctx: Context, view: LiveTurnStatus.View?): Notification {
        val s = view?.status
        val title = s?.title?.takeIf { it.isNotBlank() }
            ?: ctx.getString(R.string.notif_turn_title)
        val text = textual(ctx, view)
        val icon = iconFor(s)
        val open = openIntent(ctx)
        // 完事之后 `when` 该停在结束那一刻，而不是继续往前走
        val whenMs = s?.finishedAtMs ?: s?.startedAtMs ?: System.currentTimeMillis()

        // Android 16+ 且系统允许：走 Live Updates（状态栏那颗常驻芯片）。内容一样，
        // 只是展示面不一样 —— 不可用就退回普通通知，不是错误。
        val chip = LiveUpdates.active(ctx)
        noteSurface(chip)
        // 版本检查在这里再写一遍：`LiveUpdates.active()` 里已经有一次，但**调用点**也写明
        // 「这条路径只在 Android 16+ 走」对读代码的人和 lint 都更清楚（lint 就是这么判的）
        if (chip && Build.VERSION.SDK_INT >= Build.VERSION_CODES.BAKLAVA) {
            return promoted(ctx, title, text, icon, open, whenMs)
        }

        return Notification.Builder(ctx, CHANNEL_LIVE)
            .setSmallIcon(icon)
            .setContentTitle(title)
            .setContentText(text)
            .setStyle(Notification.BigTextStyle().bigText(text))
            .setContentIntent(open)
            .setOngoing(true)
            // 内容半秒一变，但**不许**每变一次就响一下 / 亮一下屏
            .setOnlyAlertOnce(true)
            .setShowWhen(true)
            // `when` 的两个值都是从 Rust 过来的 wall clock（不是 elapsedRealtime）——
            // OpenMinis 把 elapsedRealtime 直接喂给 setWhen，计时器的原点落在「开机
            // 时刻」，一台开了五天的手机显示 120:00:00。这个坑我们的类型分开挡掉了。
            .setWhen(whenMs)
            .setCategory(Notification.CATEGORY_SERVICE)
            .build()
    }

    /**
     * Live Updates 那一版（API 36+）。调用方已经用 [LiveUpdates.active] 把门把住了，
     * 这里才敢碰 36 才有的东西：
     *
     * - 样式用 `BigTextStyle`（16 和 17 的 `hasPromotableStyle()` 都认它 —— 17 里是
     *   BigText / Call / Metric / Progress 四种），
     * - **上色按系统版本分开写**：
     *   - Android 16（`android16-release` 的 `Notification.java`）：最后一行是
     *     `isColorizedRequested() && hasPromotableStyle()` —— **必须请求上色**；
     *   - Android 17（真机 `framework.jar` 反汇编 `hasPromotableCharacteristics`）：
     *     `… && !isColorizedRequested() && !isBridged()` —— **请求了上色反而不给提升**。
     *   两边正相反，所以按版本选。不分开写就总有一边永远拿不到芯片 —— 实测踩过：
     *   我们先按 16 写（请求上色），在 Pixel 10 / Android 17 上就是「逐项全绿、
     *   但 `hasPromotableCharacteristics()` 仍回 false」。OpenMinis 写的是
     *   `setColorized(false)`，他们对着 17，所以能出芯片。
     * - `setShortCriticalText` 给芯片那一行**更短**的文本（通知栏那两行比芯片宽），
     * - **不**当分组摘要、**不**用自定义 View —— 这两条也会取消资格。
     */
    @SuppressLint("NewApi") // 上面那个版本检查保证只在这里进来；里面用的都是 36+ 的 API
    private fun promoted(
        ctx: Context,
        title: String,
        text: String,
        icon: Int,
        open: PendingIntent,
        whenMs: Long,
    ): Notification {
        val colorize = Build.VERSION.SDK_INT < 37
        val b = Notification.Builder(ctx, CHANNEL_LIVE)
            .setSmallIcon(icon)
            .setContentTitle(title)
            .setContentText(text)
            .setStyle(Notification.BigTextStyle().bigText(text))
            .setContentIntent(open)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setShowWhen(true)
            .setWhen(whenMs)
            .setColorized(colorize)
        b.setShortCriticalText(chipText(text))
        b.addExtras(Bundle().apply { putBoolean(EXTRA_REQUEST_PROMOTED_ONGOING, true) })

        val n = b.build()
        // 提升不成功不是错误：退化成普通常驻通知，只是不上芯片。把每一项前提都打出来，
        // 真机上不提升时才知道是哪一条不满足。
        if (!n.hasPromotableCharacteristics()) {
            Log.w(TAG, "Live Update 没拿到提升资格 —— 逐项拆开看: ${promotableDetail(n)}")
        }
        return n
    }

    /**
     * 「提升资格」的每一项，摊开写成一行日志。
     *
     * 前三项能用公开 API 读；`isRequestPromotedOngoing` 也能；而 `containsCustomViews` /
     * `hasPromotableStyle` 在 17 上是 **hiddenapi BLOCKED**，反射会被拒 → 那一项就是 `null`
     * （不是 bug，看到了不必惊）。反射挂了也绝不让它把一条通知带崩（诊断日志不值这个代价）。
     *
     * 为什么要这么细：真机上「为什么没有芯片」只可能卡在某一条上，而
     * `hasPromotableCharacteristics()` 只回一个布尔 —— 不把它拆开就只能靠猜。（实踩：
     * 这台 Pixel 10 / Android 17 上我们逐项全绿却仍然 false，就是靠这行发现还差东西。）
     */
    private fun promotableDetail(n: Notification): String {
        fun hidden(name: String): Any? = runCatching {
            Notification::class.java.getMethod(name).apply { isAccessible = true }.invoke(n)
        }.getOrNull()

        return "ongoing=${(n.flags and Notification.FLAG_ONGOING_EVENT) != 0}" +
            " title=${!n.extras.getCharSequence(Notification.EXTRA_TITLE).isNullOrEmpty()}" +
            " summary=${(n.flags and Notification.FLAG_GROUP_SUMMARY) != 0}" +
            " colorized=${n.extras.getBoolean("android.colorized")}" +
            " requested=${n.extras.getBoolean(EXTRA_REQUEST_PROMOTED_ONGOING)}" +
            " template=${n.extras.getString(Notification.EXTRA_TEMPLATE)}" +
            " promotableStyle=${hidden("hasPromotableStyle")}" +
            " customViews=${hidden("containsCustomViews")}" +
            " isRequested=${hidden("isRequestPromotedOngoing")}"
    }

    /**
     * 完事那一条。
     *
     * **总是发**（不只在切后台的时候）：进行中那条的命是跟着前台服务的，服务一停它就没了 ——
     * 而「通知自己消失了」等于什么都没说，人回来看到空荡荡的通知栏，还是不知道到底跑完没有。
     * 所以回合结束时把结果留成一条**能点掉**的通知，人自己看过（或点开）才清。
     *
     * 响不响看人在不在：
     *
     * - **正看着 App**（`MainActivity.current != null`）：走 `agent-turn`（LOW，不响不弹）。
     *   他刚看完那件事，没必要再被响一声。
     * - **不在看着**：走 `agent-done`（DEFAULT，响 + 小红点）—— 这条通知就是干这个的。
     *
     * （平台没有「单条静音」这个能力：公开 SDK 里 `setSilent` 已没了，而通知的 sound 为 null 时
     * 会退回**渠道**的声音 —— 所以只能靠换渠道。）
     *
     * 发不出去也不报错：Android 13+ 没给通知权限时 `notify` 会抛 SecurityException，
     * 而这条路径在 agent 收尾的地方 —— 不能因为一条通知把收尾带崩。
     */
    fun showDone(ctx: Context, s: TurnStatus) {
        // 通道可能还没建：这条通知不依赖前台服务（服务可能压根没起来 —— 比如回合是
        // 脚本起的，Android 12+ 不允许从后台起前台服务），所以自己先把通道备好。
        // 建过就是空操作。
        ensureChannels(ctx)
        val watched = MainActivity.current != null
        val nm = ctx.getSystemService(NotificationManager::class.java) ?: return
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU && !nm.areNotificationsEnabled()) {
            return
        }
        val title = outcome(ctx, s) + " " +
            (s.title.takeIf { it.isNotBlank() } ?: ctx.getString(R.string.notif_turn_title))
        val body = s.detail ?: ctx.getString(R.string.notif_done_text)
        val n = Notification.Builder(ctx, if (watched) CHANNEL_LIVE else CHANNEL_DONE)
            .setSmallIcon(iconFor(s))
            .setContentTitle(title.trim())
            .setContentText(body)
            .setStyle(Notification.BigTextStyle().bigText(body))
            .setContentIntent(openIntent(ctx))
            .setAutoCancel(true)
            .setOnlyAlertOnce(true)
            .setShowWhen(true)
            .setWhen(s.finishedAtMs ?: System.currentTimeMillis())
            .setCategory(Notification.CATEGORY_MESSAGE)
            .build()
        try {
            nm.notify(ID_DONE, n)
        } catch (t: Throwable) {
            Log.w(TAG, "完成通知发不出去（${t.javaClass.simpleName}: ${t.message}）")
        }
    }

    /**
     * 把「完成」那条清掉。App 回到前台时调（`MainActivity.onResume`）——
     * 人都打开 App 看到结果了，通知栏里那条就没用了。
     *
     * 只清自己那一个 id：`cancelAll()` 会把前台服务那条也一起端掉。
     */
    fun clearDone(ctx: Context) {
        runCatching { ctx.getSystemService(NotificationManager::class.java)?.cancel(ID_DONE) }
            .onFailure { Log.w(TAG, "清完成通知失败: ${it.message}") }
    }

    // —— 文案与图标 ----------------------------------------------------------

    /** 第二行：此刻在干什么。 */
    private fun textual(ctx: Context, view: LiveTurnStatus.View?): String {
        val s = view?.status ?: return ctx.getString(R.string.notif_turn_text)
        val base = when (s.phase) {
            Phase.TOOL -> toolLine(ctx, s)
            Phase.STREAMING -> s.detail ?: ctx.getString(R.string.notif_phase_writing)
            Phase.THINKING -> ctx.getString(R.string.notif_phase_thinking)
            Phase.DONE -> doneLine(ctx, s)
            Phase.OTHER -> ctx.getString(R.string.notif_turn_text)
        }
        // 多会话同时跑：主会话的细节 + 还有几个（不然用户以为只有一个在跑）
        val others = (view.activeCount - 1).coerceAtLeast(0)
        return if (others > 0) base + ctx.getString(R.string.notif_more_sessions, others) else base
    }

    /** 工具那一行：「运行 apk add python3」/「读取 /etc/hosts」。 */
    private fun toolLine(ctx: Context, s: TurnStatus): String {
        val what = s.detail ?: return ctx.getString(R.string.notif_turn_text)
        return when (s.tool) {
            "bash" -> ctx.getString(R.string.notif_tool_bash, what)
            "read" -> ctx.getString(R.string.notif_tool_read, what)
            "edit" -> ctx.getString(R.string.notif_tool_edit, what)
            else -> ctx.getString(R.string.notif_tool_other, what)
        }
    }

    private fun doneLine(ctx: Context, s: TurnStatus): String {
        val how = outcome(ctx, s)
        return s.detail?.let { "$how · $it" } ?: how
    }

    private fun outcome(ctx: Context, s: TurnStatus): String = when {
        s.stopped -> ctx.getString(R.string.notif_done_stopped)
        s.ok -> ctx.getString(R.string.notif_done_ok)
        else -> ctx.getString(R.string.notif_done_failed)
    }

    /**
     * 每个阶段一个单色小图标（系统会按主题染色，所以必须是**只有 alpha 的掩码**）。
     *
     * 注：Android 17 上通知**行**里显示的是启动器图标（小图标只在状态栏/芯片里），
     * 所以这里的差别不一定在每一处都看得见 —— 那不代表它没生效（见 docs/06）。
     */
    private fun iconFor(s: TurnStatus?): Int = when {
        s == null -> R.drawable.ic_notif_work
        s.phase == Phase.DONE && s.ok && !s.stopped -> R.drawable.ic_notif_done
        s.phase != Phase.TOOL -> R.drawable.ic_notif_work
        s.tool == "bash" -> R.drawable.ic_notif_shell
        s.tool == "read" || s.tool == "edit" -> R.drawable.ic_notif_file
        else -> R.drawable.ic_notif_work
    }

    /** 点一下回到 App（和以前同一条路：不用 deep link，打开的就是刚才那个会话）。 */
    private fun openIntent(ctx: Context): PendingIntent = PendingIntent.getActivity(
        ctx,
        0,
        Intent(ctx, MainActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP),
        // IMMUTABLE：Android 12+ 必须显式指定
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )

    /**
     * 芯片那一行的文本：比通知栏窄得多，得先按**显示宽度**截。
     *
     * 中日韩字和 emoji 是「宽」字符（占两格），拉丁字母占一格 —— 按字符数截会让
     * 「运行 apk add python3」看着还行、而一串中文被芯片吃掉尾巴。
     * （口径和 OpenMinis 的 `chipContentText` 一样：最多 6 个字符、9 个宽度单位。）
     */
    private fun chipText(text: String): String {
        val clean = text.replace(Regex("\\s+"), " ").trim()
        val points = clean.codePoints().toArray()
        fun width(p: Int) = if (p >= 0x1100) 2 else 1
        if (points.size <= 7 && points.sumOf { width(it) } <= CHIP_MAX_UNITS) return clean
        var count = 0
        var units = 0
        for (p in points) {
            if (count == 6 || units + width(p) > CHIP_MAX_UNITS) break
            units += width(p)
            count++
        }
        return clean.substring(0, clean.offsetByCodePoints(0, count)) + "…"
    }
}
