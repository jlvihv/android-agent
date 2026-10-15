package dev.vihv.agent

/**
 * `--key value` / `--key=value` / 位置参数 的小解析器。
 *
 * 参数是从沙箱那个 `agent` 脚本原样转过来的（`agent sms list --limit 5`），
 * 所以形状和 shell 一样。规则：
 *
 * - `--key value` 和 `--key=value` 都认；
 * - 后面跟的是另一个 `--` 开头的参数、或者没有下一个了 → 当成布尔开关
 *   （`agent calendar list --today`）。**负数是值**（`--lon -122.4`），
 *   因为只有 `--` 才算开关的标志；
 * - 其余都是位置参数（子命令、id、查询词）。
 *
 * 与 Rust 侧 `OffloadArgs`（OpenMinis 的那个）一个精神：一个能力一个门，
 * 参数不静默丢弃 —— handler 里每个命令自己校验缺没缺，缺了报错。
 */
class PhoneArgs(argv: List<String>) {
    private val values = HashMap<String, String>()
    private val switches = HashSet<String>()

    /** 位置参数（`argv[0]` 是子命令）。 */
    val positional = ArrayList<String>()

    init {
        var i = 0
        while (i < argv.size) {
            val a = argv[i]
            when {
                a.startsWith("--") && a.contains('=') -> {
                    val k = a.substring(2).substringBefore('=')
                    values[k] = a.substringAfter('=')
                    i++
                }

                a.startsWith("--") -> {
                    val k = a.substring(2)
                    val next = argv.getOrNull(i + 1)
                    if (next != null && !next.startsWith("--")) {
                        values[k] = next
                        i += 2
                    } else {
                        switches += k
                        i++
                    }
                }

                else -> {
                    positional += a
                    i++
                }
            }
        }
    }

    /** 子命令（第一个位置参数），没有就是 null。 */
    fun sub(): String? = positional.firstOrNull()

    fun get(key: String): String? = values[key]

    /** `--help` / `-h`：每个命令都要认，别让模型猜参数。 */
    fun wantsHelp(): Boolean = switches.contains("help") || switches.contains("h") ||
        positional.contains("--help") || positional.contains("-h")

    fun has(key: String): Boolean = switches.contains(key) || values.containsKey(key)

    fun int(key: String): Int? = values[key]?.trim()?.toIntOrNull()

    fun long(key: String): Long? = values[key]?.trim()?.toLongOrNull()

    fun double(key: String): Double? = values[key]?.trim()?.toDoubleOrNull()

    /** 有 `--key` 就是 true；显式给了 `--key true/false` 按值算。 */
    fun bool(key: String): Boolean? = when (val v = values[key]?.lowercase()) {
        null -> if (switches.contains(key)) true else null
        "true", "yes", "1", "on" -> true
        "false", "no", "0", "off" -> false
        else -> null
    }
}
