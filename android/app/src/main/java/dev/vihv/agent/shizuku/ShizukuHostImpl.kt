package dev.vihv.agent.shizuku

import android.util.Log
import org.json.JSONObject
import uniffi.agent_core.HostException
import uniffi.agent_core.ShizukuHost

/**
 * 实现 Rust 的 `ShizukuHost`（`core/src/ffi.rs`）。
 *
 * **薄到几乎没有逻辑**，这是刻意的：判断（参数校验、dumpsys 解析、排版）都在
 * 核心里，那边能写单测。这里只把 argv 转成一条特权命令、跑、回原始输出。
 *
 * 两种组：
 *
 * - `service`：**不用跑到这儿**（核心自己看 [ShizukuManager.snapshot]），
 *   但留着兜底，免得核心那边改主意时这里没有入口。
 * - `exec`：`sh -c '<拼起来的命令>'`。用 `sh -c` 而不是逐个 argv 传，
 *   是为了让 `|` `>` `;` 这些元字符保持 shell 语义（和 `adb shell` 一致）。
 *
 * 返回的 JSON 形状（见 `core/src/shizuku.rs::ShizukuApi::run`）：
 * 成功 `{"ok":true,"text":stdout}`，失败 `{"ok":false,"error":{code,message}}`
 * —— 失败时**也把已经吐出来的 stdout 带上**（很多命令失败时仍有有用的输出，
 * 比如 `settings get` 查一个不存在的键）。
 */
class ShizukuHostImpl : ShizukuHost {

    companion object {
        private const val TAG = "AgentShizukuHost"

        /** 硬上限：核心那边 300s，这里留余量但不无限等。 */
        private const val MAX_TIMEOUT_MS = 280_000L
    }

    override fun `state`(): String {
        val s = ShizukuManager.snapshot()
        return JSONObject()
            .put("state", s.state.name)
            .put("version", s.version)
            .put("uid", s.uid)
            .put("sui", s.sui)
            .toString()
    }

    override fun `run`(argv: List<String>): String {
        val group = argv.firstOrNull() ?: return err("INVALID_ARGS", "没有子命令")
        return try {
            when (group) {
                "service" -> runService(argv.drop(1))
                "exec" -> runExec(argv.drop(1))
                else -> err("INVALID_ARGS", "不认识的组「$group」（核心那边加了组但这里没跟上）")
            }
        } catch (t: Throwable) {
            Log.w(TAG, "跑 $group 失败", t)
            err("OPERATION_FAILED", t.message ?: t.javaClass.simpleName)
        }
    }

    override fun requestPermission(): Boolean = ShizukuManager.requestPermission()

    // -----------------------------------------------------------------------

    private fun runService(rest: List<String>): String {
        val s = ShizukuManager.snapshot()
        return when (rest.firstOrNull() ?: "status") {
            "ping" ->
                if (s.state == ShizukuManager.State.READY) ok(s.state.name)
                else err(s.state.name, "没就绪")
            else -> ok(s.state.name)
        }
    }

    /**
     * `exec <命令…> [--timeout-ms N]`。
     *
     * 拼成一条字符串交给 `sh -c`：模型写的 `dumpsys battery | grep level`
     * 得按它写的意思跑。`--timeout-ms` 在拼之前摘掉（它不是命令的一部分）。
     */
    private fun runExec(rest: List<String>): String {
        if (rest.isEmpty()) throw IllegalArgumentException("用法：agent shizuku exec <命令…> [--timeout-ms N]")
        var timeout = 30_000L
        val cmd = mutableListOf<String>()
        var i = 0
        while (i < rest.size) {
            if (rest[i] == "--timeout-ms" && i + 1 < rest.size) {
                timeout = rest[i + 1].toLongOrNull()
                    ?: throw IllegalArgumentException("--timeout-ms 要一个数字，给的是「${rest[i + 1]}」")
                i += 2
            } else {
                cmd += rest[i]
                i += 1
            }
        }
        if (cmd.isEmpty()) throw IllegalArgumentException("用法：agent shizuku exec <命令…> [--timeout-ms N]")

        val r = ShizukuManager.runProcess(
            arrayOf("sh", "-c", cmd.joinToString(" ")),
            timeoutMs = timeout.coerceIn(1_000L, MAX_TIMEOUT_MS),
        )
        return if (r.exitCode == 0) {
            ok(r.combined)
        } else {
            // 失败也带上输出：模型要看到 pm/settings 报的那句话才知道怎么改。
            // 超时 / 被杀时给一句说清「为什么什么都没回」的话，而不是一个光秃秃的
            // 「命令失败」—— 那会让模型以为命令本身写错了，于是换个写法重试。
            val msg = r.combined.ifBlank {
                when (r.exitCode) {
                    124 -> "命令超时（${timeout}ms 没跑完，已经掐掉了）—— 要更久就把 --timeout-ms 加大"
                    143 -> "命令被杀了（exitCode=143，特权进程中途被销毁）"
                    // 外层已经写成「错误：…」，这里别再重复一遍"命令失败"
                    else -> "exitCode=${r.exitCode}，没有输出"
                }
            }
            err(codeFor(r.exitCode), msg)
        }
    }

    /** `124` 超时、`143` 被杀 —— coreutils / shell 的老约定，模型认识。 */
    private fun codeFor(exitCode: Int): String = when (exitCode) {
        126 -> "PERMISSION_DENIED"
        124 -> "TIMEOUT"
        143 -> "KILLED"
        else -> "OPERATION_FAILED"
    }

    private fun ok(text: String): String =
        JSONObject().put("ok", true).put("text", text).toString()

    private fun err(code: String, message: String): String = JSONObject()
        .put("ok", false)
        .put("error", JSONObject().put("code", code).put("message", message))
        .toString()
}
