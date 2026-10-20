package dev.vihv.agent.shizuku

import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.util.Log
import rikka.shizuku.Shizuku
import dev.vihv.agent.MainActivity
import rikka.sui.Sui
import java.util.concurrent.TimeUnit

/**
 * 以 **adb shell（uid 2000）/ root（uid 0）** 的身份跑 Android 命令。
 *
 * 三条属性，决定了下面每一行代码：
 *
 * 1. **binder 优先**。判断「能不能用」只看 `Shizuku.pingBinder()` + 授权，
 *    **不看装了哪个管理器 APK** —— Sui 是 Magisk/KernelSU 模块，根本没有 APK，
 *    先查包名会把活着的 Sui 判成「没装」。
 * 2. **执行走 `newProcess` 反射**。Shizuku 没把这个方法放进公开 API，
 *    但它是官方 manager 自己用的那条路（返回 `ShizukuRemoteProcess`，
 *    本身就是 `java.lang.Process`）。签名一变就退化成一句明确的错误，不装成功。
 * 3. **不在这里做判断**。参数怎么校验、dumpsys 怎么解析、输出怎么排版，
 *    全在 Rust 核心里（`core/src/shizuku.rs`）—— 那边能喂一段真文本写单测，
 *    这边只能靠真机手点。
 *
 * 会被 Rust 从 `spawn_blocking` 线程调用（`ShizukuHostImpl`），所以这里的
 * 阻塞没有关系；但**不能碰 UI 线程的东西**，只有 `requestPermission` 要切过去。
 */
object ShizukuManager {
    private const val TAG = "AgentShizuku"

    /** 状态机的四个格子。顺序有意义：`READY` 是唯一能干活的那个。 */
    enum class State { NOT_INSTALLED, NOT_RUNNING, NEED_PERMISSION, READY }

    data class Snapshot(
        val state: State,
        val version: Int = -1,
        val uid: Int = -1,
        /** binder 是 Sui（root 模块）给的吗 —— 只影响文案 */
        val sui: Boolean = false,
    )

    data class ProcessResult(
        val exitCode: Int,
        val stdout: String,
        val stderr: String,
    ) {
        /** stdout + stderr（两者都可能为空；两边都 trim 过，不会拼出开头一个空行）。 */
        val combined: String
            get() = listOf(stdout, stderr).filter { it.isNotBlank() }.joinToString("\n")
    }

    /** 请求码：和 MainActivity（2..5）、AgentPermissions（0x4A11）、PermissionCenter（0x4A12）错开 */
    const val REQUEST_CODE = 0x4A13

    /** 已知的 Shizuku 协议管理器包名。**Sui 故意不在里面**（它是模块，没有 APK）。 */
    private val MANAGER_PACKAGES = listOf(
        "moe.shizuku.privileged.api", // Shizuku
        "frb.axeron.manager", // AXManager
    )

    private const val SHIZUKU_RELEASES = "https://github.com/RikkaApps/Shizuku/releases"
    private const val SUI_URL = "https://github.com/RikkaApps/Sui"

    @Volatile
    private var appContext: Context? = null

    @Volatile
    private var listenersRegistered = false

    /** 状态变了要通知谁（`PermissionsSection` 那类界面刷新用；目前没人订阅，留着给壳） */
    @Volatile
    private var onChange: (() -> Unit)? = null

    /**
     * 初始化。`AgentApp.onCreate` 里调一次。
     *
     * 只注册监听器，不碰 binder —— binder 什么时候来是 Shizuku 决定的
     * （它可能比我们的 Application 还晚）。所有判断都是**现问现答**的。
     */
    fun init(context: Context, onChange: (() -> Unit)? = null) {
        appContext = context.applicationContext
        this.onChange = onChange
        if (listenersRegistered) return
        listenersRegistered = true
        runCatching {
            Shizuku.addBinderReceivedListenerSticky {
                Log.i(TAG, "binder received")
                onChange?.invoke()
            }
            Shizuku.addBinderDeadListener {
                Log.w(TAG, "binder died")
                onChange?.invoke()
            }
        }.onFailure { Log.w(TAG, "注册 binder 监听失败", it) }
    }

    // -----------------------------------------------------------------------
    // 状态
    // -----------------------------------------------------------------------

    private fun binderAlive(): Boolean =
        runCatching { Shizuku.pingBinder() }.getOrDefault(false)

    private fun permitted(): Boolean = runCatching {
        Shizuku.pingBinder() && Shizuku.checkSelfPermission() == PackageManager.PERMISSION_GRANTED
    }.getOrDefault(false)

    private fun isSui(): Boolean = runCatching { Sui.isSui() }.getOrDefault(false)

    private fun managerInstalled(): Boolean {
        val ctx = appContext ?: return false
        return MANAGER_PACKAGES.any { pkg ->
            runCatching { ctx.packageManager.getPackageInfo(pkg, 0); true }.getOrDefault(false)
        }
    }

    /**
     * 状态机。**顺序就是全部逻辑**（照抄 OpenMinis 的 binder-first 那版，
     * 并把它写成纯函数，好单测）：
     *
     * 1. binder 活着 → READY / NEED_PERMISSION
     * 2. binder 没了，但装了管理器 APK → NOT_RUNNING（去启动它）
     * 3. 都没有 → NOT_INSTALLED（去装）
     */
    fun decideState(
        binderAlive: Boolean,
        permitted: Boolean,
        managerInstalled: Boolean,
    ): State = when {
        binderAlive && permitted -> State.READY
        binderAlive -> State.NEED_PERMISSION
        managerInstalled -> State.NOT_RUNNING
        else -> State.NOT_INSTALLED
    }

    /** 现在是什么状态。**不许抛** —— 拿不到就回 NOT_INSTALLED。 */
    fun snapshot(): Snapshot = runCatching {
        val alive = binderAlive()
        val state = decideState(alive, alive && permitted(), managerInstalled())
        if (state == State.READY) {
            Snapshot(
                state = state,
                version = runCatching { Shizuku.getVersion() }.getOrDefault(-1),
                uid = runCatching { Shizuku.getUid() }.getOrDefault(-1),
                sui = isSui(),
            )
        } else {
            Snapshot(state = state, sui = alive && isSui())
        }
    }.getOrElse {
        Log.w(TAG, "取状态失败", it)
        Snapshot(State.NOT_INSTALLED)
    }

    fun isReady(): Boolean = snapshot().state == State.READY

    // -----------------------------------------------------------------------
    // 授权
    // -----------------------------------------------------------------------

    /**
     * 拉一次授权界面。返回「请求发出去了没有」（不是「授没授权」——
     * 结果得之后再看 [snapshot]）。
     *
     * 两个必须：**主线程**（Shizuku 内部会 startActivity）、**有 Activity**
     * （后台弹不出界面，用户只会看到一句失败）。
     */
    fun requestPermission(): Boolean {
        if (!binderAlive()) return false
        if (permitted()) return true
        // 用 `alive` 而不是 `current`：授权界面是 startActivity，
        // 「我们刚打开过别的界面（paused）」不该成为拉不起来的理由（见 docs/13 §5）
        val activity = MainActivity.alive ?: run {
            Log.w(TAG, "没人在前台，拉不起授权界面")
            return false
        }
        return runCatching {
            // shouldShowRequestPermissionRationale 为 true = 用户永久拒绝过，
            // 再请求不会弹窗；这时候只能让用户去 Shizuku App 里自己开
            if (Shizuku.shouldShowRequestPermissionRationale()) {
                Log.w(TAG, "授权被永久拒绝过，得去 Shizuku App 里手动开")
                return false
            }
            activity.runOnUiThread {
                runCatching { Shizuku.requestPermission(REQUEST_CODE) }
                    .onFailure { Log.w(TAG, "requestPermission 失败", it) }
            }
            true
        }.getOrDefault(false)
    }

    // -----------------------------------------------------------------------
    // 执行
    // -----------------------------------------------------------------------

    /**
     * 跑一条命令。`argv` 是**已经拼好的** argv（`["sh","-c","id"]`、
     * `["pm","list","packages"]`），这里不再拆参数。
     *
     * `exitCode` 约定：`124` 超时、`143` 被杀、`126` 没授权 —— 都是 coreutils /
     * shell 的老约定，模型和脚本都认识这几个数。
     */
    fun runProcess(argv: Array<String>, timeoutMs: Long = 30_000L): ProcessResult {
        val snap = snapshot()
        if (snap.state != State.READY) {
            return ProcessResult(
                exitCode = 126,
                stdout = "",
                stderr = "shizuku 没就绪（${snap.state}）",
            )
        }
        val proc = newProcess(argv)
            ?: return ProcessResult(1, "", "拿不到特权进程（Shizuku.newProcess 调不通）")

        val out = StringBuilder()
        val err = StringBuilder()
        val outThread = drain(proc.inputStream, out)
        val errThread = drain(proc.errorStream, err)

        val exited = waitFor(proc, timeoutMs)
        if (!exited) {
            // **先杀再收尾**：反过来的话，收尾要等那 2 秒，而进程还在往里写 ——
            // 用户/模型看到的是「超时」却带着一段跑来之后才有的输出（实测：`sleep 5;
            // echo done` 带 `--timeout-ms 2000` 回了 `done`，等于骗了人）。
            runCatching { proc.destroy() }
        }
        runCatching { outThread.join(2_000) }
        runCatching { errThread.join(2_000) }

        val stdout = out.toString().trimEnd()
        val stderr = err.toString().trimEnd()
        if (!exited) {
            // 超时：输出只留已经吐出来的那部分，并且**明说没跑完**
            return ProcessResult(124, stdout, stderr)
        }
        // exitValue 在进程刚退出时可能有一瞬还抛 IllegalThreadStateException
        val code = runCatching { proc.exitValue() }.getOrDefault(-1)
        return ProcessResult(code, stdout, stderr)
    }

    /**
     * `Shizuku.newProcess(argv, env, cwd)` —— 隐藏 API，反射调。
     *
     * 三条都试：三参数 → 两参数 → 一参数。不同版本签名不一样，
     * 而**拿不到就得说清楚**（返回 null，调用方翻成一句人话），不能假装成功。
     */
    private fun newProcess(argv: Array<String>): Process? {
        val cls = Shizuku::class.java
        val attempts: List<Array<Class<*>>> = listOf(
            arrayOf(Array<String>::class.java, Array<String>::class.java, String::class.java),
            arrayOf(Array<String>::class.java, Array<String>::class.java),
            arrayOf(Array<String>::class.java),
        )
        for (sig in attempts) {
            val args: Array<Any?>? = when (sig.size) {
                3 -> arrayOf(argv, null, null)
                2 -> arrayOf(argv, null)
                else -> arrayOf(argv)
            }
            val p = runCatching {
                val m = cls.getDeclaredMethod("newProcess", *sig)
                m.isAccessible = true
                m.invoke(null, *args!!) as? Process
            }.onFailure { Log.w(TAG, "newProcess/${sig.size} 不可用：${it.message}") }
                .getOrNull()
            if (p != null) return p
        }
        return null
    }

    /**
     * 等进程退出。返回**真的退出了没有**。
     *
     * 为什么要自己实现：Shizuku 的 `RemoteProcess.waitFor(long, TimeUnit)`
     * 在 13.x 上会**抛** `IllegalArgumentException("process hasn't exited")`
     * 而不是阻塞（SDK 的 bug）。所以退化成轮询 `exitValue()`。
     */
    private fun waitFor(proc: Process, timeoutMs: Long): Boolean = try {
        proc.waitFor(timeoutMs, TimeUnit.MILLISECONDS)
    } catch (t: Throwable) {
        if (t !is IllegalArgumentException && t !is IllegalThreadStateException) {
            Log.w(TAG, "waitFor 出意外，改轮询：${t.message}")
        }
        val deadline = System.currentTimeMillis() + timeoutMs
        var done = false
        while (System.currentTimeMillis() < deadline) {
            try {
                proc.exitValue()
                done = true
                break
            } catch (_: IllegalThreadStateException) {
                Thread.sleep(50)
            } catch (_: IllegalArgumentException) {
                Thread.sleep(50)
            }
        }
        done
    }

    private fun drain(stream: java.io.InputStream?, into: StringBuilder): Thread =
        Thread {
            runCatching {
                stream?.bufferedReader()?.forEachLine { line ->
                    synchronized(into) { into.append(line).append('\n') }
                }
            }
        }.apply {
            isDaemon = true
            start()
        }

    // -----------------------------------------------------------------------
    // 设置页入口
    // -----------------------------------------------------------------------

    /** 打开管理器 App（Shizuku / AXManager）。Sui 没有界面，直接开安装页。 */
    fun openManager(context: Context) {
        val pkg = MANAGER_PACKAGES.firstOrNull { p ->
            runCatching { context.packageManager.getPackageInfo(p, 0); true }.getOrDefault(false)
        }
        val intent = if (pkg != null) {
            context.packageManager.getLaunchIntentForPackage(pkg)
                ?.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        } else {
            null
        } ?: Intent(Intent.ACTION_VIEW, Uri.parse(if (isSui()) SUI_URL else SHIZUKU_RELEASES))
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        runCatching { context.startActivity(intent) }
            .onFailure { Log.w(TAG, "打不开管理器", it) }
    }

    /** 给人看的一句话（设置页那一栏）。 */
    fun describe(): String = when (val s = snapshot().state) {
        State.READY -> "已连接（uid=${snapshot().uid}）"
        State.NEED_PERMISSION -> "已安装，未授权"
        State.NOT_RUNNING -> "已安装，未运行"
        State.NOT_INSTALLED -> "未安装"
    }
}
