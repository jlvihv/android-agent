package dev.vihv.agent

import android.util.Log
import org.json.JSONObject
import uniffi.agent_core.CoreException
import uniffi.agent_core.DeviceHost

/**
 * 实现 Rust 定义的 `DeviceHost` callback interface（决策 D2）。
 *
 * uniffi 自动生成了 `DeviceHost` 接口和**反向调用**的胶水代码 ——
 * 这里不需要写一行 JNI。这是选 UniFFI 换来的东西。
 *
 * 边界用 JSON 字符串而不是强类型：Rust 侧的 `Command` 枚举序列化过来，
 * 这里按 `type` 分发。加新指令只改两边各自的 match，不用动 FFI 定义。
 */
class DeviceHostImpl(
    private val service: AgentAccessibilityService,
) : DeviceHost {

    companion object {
        private const val TAG = "AgentDeviceHost"
    }

    /**
     * 由 Rust 从 `spawn_blocking` 线程调用。
     * 抛 `CoreException.Failed` 表示失败，Rust 侧会收到 Err。
     */
    override fun execute(cmdJson: String): String {
        val cmd = try {
            JSONObject(cmdJson)
        } catch (t: Throwable) {
            throw CoreException.Failed("指令不是合法 JSON: ${t.message}")
        }

        return try {
            when (val type = cmd.getString("type")) {
                "tap_xy" -> service.tapXY(cmd.getInt("x"), cmd.getInt("y"))
                "swipe" -> service.swipe(
                    cmd.getInt("x1"), cmd.getInt("y1"),
                    cmd.getInt("x2"), cmd.getInt("y2"),
                    cmd.optInt("ms", 300),
                )
                "key" -> service.pressKey(cmd.getString("name"))
                "launch" -> service.launch(cmd.getString("package"))
                else -> throw CoreException.Failed("未知指令: $type")
            }
        } catch (e: CoreException) {
            throw e
        } catch (t: Throwable) {
            Log.w(TAG, "指令执行失败: $cmdJson", t)
            throw CoreException.Failed(t.message ?: t.toString())
        }
    }

    override fun name(): String = "accessibility"
}
