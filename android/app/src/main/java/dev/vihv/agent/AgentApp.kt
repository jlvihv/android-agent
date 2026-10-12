package dev.vihv.agent

import android.app.Application
import android.content.Context
import android.net.ConnectivityManager
import android.os.Build
import android.util.Log
import java.io.File
import uniffi.agent_core.AgentCore
import uniffi.agent_core.AndroidEnv
import java.util.TimeZone

/**
 * 应用入口。核心生命周期就在这里 —— 跟 Application 一致，
 * 这样无障碍服务和 WebView 都拿得到同一个 `AgentCore`。
 */
class AgentApp : Application() {

    companion object {
        private const val TAG = "AgentApp"

        @Volatile
        private var instance: AgentApp? = null

        /** Rust 核心。`onCreate` 之后一定非空。 */
        @Volatile
        lateinit var core: AgentCore
            private set

        /** 实际监听的端口（0 表示还没起来） */
        @Volatile
        var port: Int = 0
            private set

        /**
         * 本地 API 的凭据。WebView 先打开 `/auth?t=<它>` 种上 cookie，
         * 之后 `/api/…` 才放行。
         *
         * 为什么要它：手机上任何有网络权限的 App 都能连 127.0.0.1，扫一遍端口
         * 就能找到核心、在沙箱里跑命令（沙箱挂着整个内部存储）。
         * 存在会话库里，跨重启不变。
         */
        @Volatile
        var token: String? = null
            private set

        /**
         * 无障碍服务连上时调用。
         *
         * 注意时序：服务可能在 Application.onCreate 之前/之后连上，
         * 两种顺序都要能工作，所以这里是幂等的。
         */
        fun onDeviceReady(service: AgentAccessibilityService) {
            if (!::core.isInitialized) {
                Log.w(TAG, "核心还没起来，设备注入延后")
                return
            }
            runCatching {
                core.setDevice(DeviceHostImpl(service))
                core.log("info", "设备已接入（无障碍服务）")
            }.onFailure { Log.e(TAG, "注入设备失败", it) }
        }

        fun onDeviceGone() {
            if (!::core.isInitialized) return
            runCatching {
                core.clearDevice()
                core.log("warn", "设备已断开（无障碍服务关闭）")
            }
        }

        fun context(): Context? = instance

        /** 有没有权限直接读写整个内部存储 */
        fun hasAllFilesAccess(ctx: Context): Boolean =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                android.os.Environment.isExternalStorageManager()
            } else {
                ctx.checkSelfPermission(android.Manifest.permission.WRITE_EXTERNAL_STORAGE) ==
                    android.content.pm.PackageManager.PERMISSION_GRANTED
            }

        /** 手机内部存储根目录；没权限时 null（沙箱的 /workspace 退回 App 私有目录） */
        fun sharedStorage(ctx: Context): String? =
            if (hasAllFilesAccess(ctx)) {
                @Suppress("DEPRECATION")
                android.os.Environment.getExternalStorageDirectory().absolutePath
            } else {
                null
            }

        /** 权限状态变了就重挂沙箱存储（Rust 侧没变化时什么都不做） */
        fun syncSharedStorage(ctx: Context) {
            if (!::core.isInitialized) return
            runCatching { core.setSharedStorage(sharedStorage(ctx)) }
                .onFailure { Log.w(TAG, "重挂存储失败", it) }
        }
    }

    /**
     * ⚠️ **这个函数绝不能抛异常。**
     *
     * 实测教训：在里面调 `startForegroundService()` 抛了
     * `ForegroundServiceStartNotAllowedException`（Android 12+ 禁止从
     * Application.onCreate 起前台服务，那时 uid 状态还是 CEM），
     * 结果整个 App 直接 FATAL。
     *
     * 所以每一步都包起来：任何一步失败都只降级，不致命。
     */
    override fun onCreate() {
        super.onCreate()
        instance = this

        // 1. 建核心（触发 System.loadLibrary("agent_core")）
        //    这一步失败就没得玩了，但也只记日志，让 Activity 显示错误页
        runCatching {
            core = AgentCore()
            Log.i(TAG, "agent-core ${core.version()}")
        }.onFailure {
            Log.e(TAG, "核心加载失败", it)
            return
        }

        // 1b. 接上浏览器。
        //
        //     **刻意不和「设备」（无障碍）绑在一起**：浏览器不需要无障碍权限，
        //     关掉无障碍也该能用。WebView 本身是懒建的 —— 不调 `browser` 工具
        //     就不会多出这只 WebView。
        runCatching { core.setBrowser(BrowserController(this)) }
            .onFailure { Log.w(TAG, "浏览器注入失败", it) }

        // 2. 配 Linux 沙箱。
        //    安卓上 proot 必须住在 nativeLibraryDir（那里才允许 exec），
        //    rootfs 在 filesDir（靠 proot 的 loader mmap，不靠 exec）。
        //    见 docs/02-linux-sandbox.md。
        runCatching { core.setupLinux(androidEnv()) }
            .onFailure { core.log("warn", "Linux 沙箱配置失败: ${it.message}") }

        // 2b. 首次启动解压 rootfs。
        //     ~500 个条目，几秒钟。放后台线程 —— 卡在 Application.onCreate
        //     里会直接 ANR。解压完成前调用 bash 会报"沙箱未就绪"，
        //     日志里能看到进度。
        if (!RootfsInstaller.isInstalled(this)) {
            core.log("info", "首次启动，正在解压 Alpine rootfs…")
            Thread({
                val ok = RootfsInstaller.installIfNeeded(this)
                if (ok) {
                    // ⚠️ 必须**再调一次** setupLinux。
                    //
                    // 第一次调的时候 rootfs 还不存在（解压在后台），
                    // 而解压会 deleteRecursively 整个目录重建 ——
                    // 把 prepare_rootfs 写进去的 /etc/resolv.conf、/root、/tmp
                    // 全冲掉了。
                    //
                    // 少了 resolv.conf 的后果：apk 解析不了域名，报
                    // "no such package"，看起来像软件源挂了。
                    // （实测踩过）
                    runCatching { core.setupLinux(androidEnv()) }
                        .onFailure { core.log("warn", "解压后重配沙箱失败: ${it.message}") }
                    core.log("info", "Linux 沙箱就绪")
                } else {
                    core.log("error", "rootfs 解压失败，bash/edit 不可用")
                }
            }, "rootfs-install").apply { isDaemon = true }.start()
        } else {
            core.log("info", "Linux 沙箱就绪")
        }

        // 3. 起 HTTP 服务。传 0 让系统分配端口 —— 硬编码端口在真机上
        //    可能被别的 App 占了。
        // setupLinux 里打开了会话库，凭据就在那里
        token = runCatching { core.token() }.getOrNull()
        token?.let { t ->
            // 调试脚本（scripts/phone-token.sh）拿它：debug 包 run-as 读文件，
            // release 包只能看 logcat —— 别的 App 读不到我们的日志，只有 adb 能。
            runCatching { File(filesDir, "token").writeText(t) }
            Log.i(TAG, "agent token: $t")
        }

        runCatching { port = core.start(0u.toUShort()).toInt() }
            .onSuccess {
                Log.i(TAG, "WebView 应指向 http://127.0.0.1:$port")
                // 把端口落到 files/port。调试时 `adb shell run-as <pkg> cat
                // files/port` 一条命令就拿到，不用去翻 logcat
                // （logcat 环形缓冲会被冲掉 —— 实测踩过）。
                // 见 scripts/phone-port.sh / docs/07-debugging.md。
                runCatching { File(filesDir, "port").writeText(port.toString()) }
            }
            .onFailure {
                Log.e(TAG, "核心启动失败", it)
                port = 0
                runCatching { File(filesDir, "port").delete() }
            }

        // 4. 如果无障碍服务已经连上了（进程重启的情况），补一次注入
        runCatching { AgentAccessibilityService.instance?.let { onDeviceReady(it) } }

        // 注意：前台服务**不在这里起**。
        // Android 12+ 会抛 ForegroundServiceStartNotAllowedException ——
        // Application.onCreate 时 App 还没进入前台。改到 MainActivity.onResume。
    }

    private fun androidEnv(): AndroidEnv {
        val info = applicationInfo
        return AndroidEnv(
            nativeLibDir = info.nativeLibraryDir,
            filesDir = filesDir.absolutePath,
            cacheDir = cacheDir.absolutePath,
            dnsServers = systemDnsServers(),
            tzOffsetMinutes = tzOffsetMinutes(),
            sharedStorage = sharedStorage(this),
        )
    }

    /**
     * 系统 DNS。
     *
     * **必须传**：Alpine minirootfs 里没有 `/etc/resolv.conf`，不写的话
     * `apk add` 会因为解析不了域名而报 "no such package" —— 看起来像
     * 软件源挂了，其实是 DNS 根本没配。实测踩过这个坑。
     */
    private fun systemDnsServers(): List<String> = runCatching {
        val cm = getSystemService(ConnectivityManager::class.java) ?: return emptyList()

        // 实测踩过：直接用 activeNetwork 的 DNS 会拿到 VPN（Tailscale）的
        // 172.19.0.2 / fdfe:...，而**这个 App 的流量不走 VPN**，
        // 于是沙箱里每条 DNS 查询都超时 → apk 报 "no such package"。
        //
        // 所以按可达性排序：非 VPN 网络优先，VPN 次之，公网兜底。
        val nonVpn = mutableListOf<String>()
        val vpn = mutableListOf<String>()
        for (net in cm.allNetworks) {
            val lp = cm.getLinkProperties(net) ?: continue
            val caps = cm.getNetworkCapabilities(net)
            val isVpn = caps?.hasTransport(android.net.NetworkCapabilities.TRANSPORT_VPN) == true
            val addrs = lp.dnsServers.mapNotNull { it.hostAddress }
            if (isVpn) vpn += addrs else nonVpn += addrs
        }

        (nonVpn + vpn).distinct().take(3) + listOf("1.1.1.1", "8.8.8.8")
    }.getOrDefault(listOf("1.1.1.1", "8.8.8.8"))

    /**
     * 当前时区偏移（分钟）。
     *
     * 沙箱里读不到宿主时区，只能外部注入。注意 Rust 侧会把它转成
     * POSIX 格式（符号与 UTC 相反）—— musl 的 TZ 解析器认不出 "GMT+8"。
     */
    private fun tzOffsetMinutes(): Int =
        TimeZone.getDefault().getOffset(System.currentTimeMillis()) / 60_000
}
