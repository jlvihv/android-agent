package dev.vihv.agent

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import android.util.Log
import java.net.Inet4Address

/**
 * 在局域网里找电脑上的核心（宿主模式的 `just dev-host` 用 mDNS 广播 `_agent-core._tcp`，
 * 见 core 的 `desktop::advertise`）。
 *
 * **只在「连接电脑」那一栏开着的时候找**：页面挂上时 `start`、关掉时 `stop`
 * （App 切到后台也停，回来再接着找）。不常驻 —— 多播发现费电，也没必要。
 *
 * 用平台的 `NsdManager`（不引第三方库，也不用自己开 MulticastLock：系统的
 * mdnsd 替我们收包）。解析（拿 IP / 端口 / TXT）旧接口一次只能解析一个，
 * 所以排个队。
 *
 * 回调来自系统的 binder 线程；`onChange` 由调用方自己切回主线程。
 */
class CoreDiscovery(context: Context, private val onChange: (List<Found>) -> Unit) {

    /** 一台找到的电脑。`id` 是核心实例的 id（存在电脑的库里，IP 变了它不变） */
    data class Found(val name: String, val id: String, val host: String, val port: Int)

    companion object {
        private const val TAG = "CoreDiscovery"
        private const val TYPE = "_agent-core._tcp"
    }

    private val nsd = context.getSystemService(NsdManager::class.java)
    private val lock = Any()
    /** 服务名 → 解析结果 */
    private val found = LinkedHashMap<String, Found>()
    private val queue = ArrayDeque<NsdServiceInfo>()
    private var resolving = false
    private var listener: NsdManager.DiscoveryListener? = null

    val running: Boolean get() = listener != null

    fun current(): List<Found> = synchronized(lock) { found.values.toList() }

    fun start() {
        if (listener != null || nsd == null) return
        val l = object : NsdManager.DiscoveryListener {
            override fun onDiscoveryStarted(serviceType: String?) {
                Log.i(TAG, "开始找 $TYPE")
            }

            override fun onDiscoveryStopped(serviceType: String?) {
                Log.i(TAG, "停止找")
            }
            override fun onStartDiscoveryFailed(serviceType: String?, errorCode: Int) {
                Log.w(TAG, "开始找失败：$errorCode")
                listener = null
            }
            override fun onStopDiscoveryFailed(serviceType: String?, errorCode: Int) {
                Log.w(TAG, "停止找失败：$errorCode")
            }

            override fun onServiceFound(info: NsdServiceInfo) {
                synchronized(lock) { queue.addLast(info) }
                resolveNext()
            }

            override fun onServiceLost(info: NsdServiceInfo) {
                val changed = synchronized(lock) { found.remove(info.serviceName) != null }
                if (changed) onChange(current())
            }
        }
        listener = l
        runCatching { nsd.discoverServices(TYPE, NsdManager.PROTOCOL_DNS_SD, l) }
            .onFailure {
                Log.w(TAG, "discoverServices 抛了", it)
                listener = null
            }
    }

    fun stop() {
        val l = listener ?: return
        listener = null
        runCatching { nsd?.stopServiceDiscovery(l) }
        synchronized(lock) {
            found.clear()
            queue.clear()
        }
    }

    private fun resolveNext() {
        val next = synchronized(lock) {
            if (resolving || listener == null) return
            queue.removeFirstOrNull()?.also { resolving = true }
        } ?: return
        @Suppress("DEPRECATION") // 新接口（registerServiceInfoCallback）要 API 34；这个两边都能用
        nsd?.resolveService(
            next,
            object : NsdManager.ResolveListener {
                override fun onResolveFailed(info: NsdServiceInfo?, errorCode: Int) {
                    Log.w(TAG, "解析 ${info?.serviceName} 失败：$errorCode")
                    done()
                }

                override fun onServiceResolved(info: NsdServiceInfo) {
                    val host = hostOf(info)
                    val id = info.attributes["id"]?.let { String(it) }.orEmpty()
                    if (host != null && id.isNotEmpty() && listener != null) {
                        synchronized(lock) { found[info.serviceName] = Found(info.serviceName, id, host, info.port) }
                        onChange(current())
                    }
                    done()
                }
            },
        ) ?: done()
    }

    private fun done() {
        synchronized(lock) { resolving = false }
        resolveNext()
    }

    /** 优先 IPv4（WebView 拼 URL 不用加方括号，也是局域网里最常见的那条路） */
    private fun hostOf(info: NsdServiceInfo): String? {
        val addrs = if (Build.VERSION.SDK_INT >= 34) {
            info.hostAddresses
        } else {
            @Suppress("DEPRECATION")
            listOfNotNull(info.host)
        }
        val pick = addrs.firstOrNull { it is Inet4Address } ?: addrs.firstOrNull()
        return pick?.hostAddress
    }
}
