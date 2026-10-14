package dev.vihv.agent

import android.content.Context
import android.util.Log
import java.io.File
import java.io.InputStream
import java.nio.file.Files
import java.nio.file.attribute.PosixFilePermission
import java.util.zip.GZIPInputStream

/**
 * 首次启动时把 Alpine rootfs 从 assets 解到 `filesDir/alpine-rootfs`。
 *
 * 为什么手写 tar 解析而不用 `commons-compress`：
 * - 不想为一件一次性的事加一个依赖
 * - tar 格式很简单（512 字节头 + 512 对齐的数据块）
 *
 * 两个必须处理的坑：
 *
 * 1. **AAPT 可能把 `alpine-minirootfs.tar.gz` 改名成 `.tar`**（它会把
 *    已压缩资源解压一层）。所以两个名字都试，并且**按 magic bytes
 *    判断是不是 gzip**（`1f 8b`），而不是按文件名。
 * 2. **不要 chmod +x。** 见下面 `installIfNeeded` 的说明。
 */
object RootfsInstaller {

    private const val TAG = "Rootfs"
    private const val DIR = "alpine-rootfs"
    private const val MARKER = ".installed"

    /**
     * 解压逻辑的版本号。改了就 bump —— 已装过的设备会强制重解。
     *
     * v2：开始应用 tar 头里的权限位（v1 没设执行位，proot 会拒绝执行）
     */
    private const val FORMAT = "v2"

    /** AAPT 可能给出其中任意一个名字 */
    private val ASSETS = listOf("alpine-minirootfs.tar.gz", "alpine-minirootfs.tar")

    private const val BLOCK = 512

    fun rootfsDir(ctx: Context): File = File(ctx.filesDir, DIR)

    fun isInstalled(ctx: Context): Boolean {
        val marker = File(rootfsDir(ctx), MARKER)
        if (!marker.isFile) return false
        // 格式变了就重解，否则会留着一个半坏的 rootfs
        return runCatching { marker.readText().startsWith(FORMAT) }.getOrDefault(false)
    }

    /**
     * 解压（幂等）。返回是否可用。
     *
     * **必须应用 tar 头里的权限位。**
     *
     * 我原本以为不需要 —— 理由是 proot 的 ELF loader 把 rootfs 里的二进制
     * mmap 进内存再跳进去，而 mmap 只要求文件可读。理论上对，但**实际上
     * proot 自己会先检查执行位**：`src/path/path.c` 的 `which()` 里
     *
     * ```c
     * if (is_explicit && (statr.st_mode & S_IXUSR) == 0) {
     *     note(tracee, ERROR, USER, "'%s' is not executable", command);
     * ```
     *
     * 不 chmod 的话 proot 在 execve 之前就直接拒了，报
     * `proot error: '/bin/sh' is not executable`。
     *
     * Android 允许 App 给自己的私有文件 chmod（SELinux 只拦 execve），
     * 所以这个能work。
     */
    fun installIfNeeded(ctx: Context): Boolean {
        if (isInstalled(ctx)) {
            Log.i(TAG, "rootfs 已就绪: ${rootfsDir(ctx)}")
            return true
        }

        val dest = rootfsDir(ctx)
        val started = System.currentTimeMillis()
        try {
            if (dest.exists()) dest.deleteRecursively()
            if (!dest.mkdirs()) error("建不了目录 ${dest.absolutePath}")

            val name = ASSETS.firstOrNull { runCatching { ctx.assets.open(it).close() }.isSuccess }
                ?: error("assets 里找不到 ${ASSETS.joinToString(" 或 ")}")

            Log.i(TAG, "从 assets/$name 解压 rootfs …")
            val raw = ctx.assets.open(name)
            val stream = if (isGzip(raw)) {
                GZIPInputStream(raw, 64 * 1024)
            } else {
                Log.i(TAG, "（AAPT 已经解掉 gzip，按裸 tar 处理）")
                raw
            }

            val count = stream.use { extract(it, dest) }

            File(dest, MARKER).writeText(
                "$FORMAT\nalpine-minirootfs\n$count entries\n${System.currentTimeMillis()}\n",
            )
            val ms = System.currentTimeMillis() - started
            Log.i(TAG, "rootfs 解压完成：$count 个条目，${ms}ms")
            return true
        } catch (t: Throwable) {
            Log.e(TAG, "rootfs 解压失败", t)
            runCatching { dest.deleteRecursively() }
            return false
        }
    }

    // -----------------------------------------------------------------------
    // tar
    // -----------------------------------------------------------------------

    private fun extract(input: InputStream, dest: File): Int {
        val header = ByteArray(BLOCK)
        var longName: String? = null
        var count = 0

        while (true) {
            if (!readFully(input, header)) break
            // 连续两个全零块表示归档结束
            if (header.all { it == 0.toByte() }) break

            val type = header[156].toInt().toChar()
            val size = readOctal(header, 124, 12)

            // GNU 长文件名：数据区就是名字
            if (type == 'L') {
                longName = String(readData(input, size), Charsets.UTF_8).trimEnd('\u0000')
                continue
            }
            // pax 扩展头：跳过（Alpine 的 minirootfs 一般不用，
            // 但 tar 实现不同就可能出现）
            if (type == 'x' || type == 'g') {
                skipData(input, size)
                continue
            }

            val name = longName ?: readName(header)
            longName = null

            val target = resolve(dest, name)
            if (target == null) {
                Log.w(TAG, "跳过越界条目: $name")
                skipData(input, size)
                continue
            }

            val mode = readOctal(header, 100, 8).toInt()

            when (type) {
                '5' -> {
                    target.mkdirs()
                    applyMode(target, mode)
                }

                '2' -> {
                    // 符号链接。Alpine 里大量条目指向**绝对路径**
                    // （/bin/ls -> /bin/busybox）。这在沙箱内是正确的，
                    // 在宿主上会解析失败 —— 所以别用 File.exists() 去判断
                    // rootfs 是否有效。
                    val link = readString(header, 157, 100)
                    target.parentFile?.mkdirs()
                    runCatching {
                        target.delete()
                        java.nio.file.Files.createSymbolicLink(
                            target.toPath(),
                            java.nio.file.Paths.get(link),
                        )
                    }.onFailure { Log.w(TAG, "建符号链接失败 $name -> $link: ${it.message}") }
                }

                '1' -> {
                    // 硬链接。Android 的 /data 拒绝跨目录硬链接，
                    // 退化成复制（内容和目标一样）
                    val link = readString(header, 157, 100)
                    val src = resolve(dest, link)
                    if (src != null && src.isFile) {
                        target.parentFile?.mkdirs()
                        src.copyTo(target, overwrite = true)
                    }
                    skipData(input, size)
                }

                else -> {
                    // '0' / '\0' 普通文件
                    target.parentFile?.mkdirs()
                    target.outputStream().use { out ->
                        copyN(input, out, size)
                    }
                    // 关键：不设执行位 proot 会拒绝执行（见方法注释）
                    applyMode(target, mode)
                }
            }

            skipPadding(input, size)
            count++
            if (count % 100 == 0) Log.i(TAG, "  … $count 个条目")
        }

        return count
    }

    /**
     * 应用 tar 头里的八进制权限位。
     *
     * Java 的 `File.setExecutable` 只能设 owner 的位，不够用 ——
     * 用 `Files.setPosixFilePermissions` 设完整的 9 位。
     */
    private fun applyMode(f: File, mode: Int) {
        if (mode == 0) return
        val perms = buildSet {
            if (mode and 0b100_000_000 != 0) add(PosixFilePermission.OWNER_READ)
            if (mode and 0b010_000_000 != 0) add(PosixFilePermission.OWNER_WRITE)
            if (mode and 0b001_000_000 != 0) add(PosixFilePermission.OWNER_EXECUTE)
            if (mode and 0b000_100_000 != 0) add(PosixFilePermission.GROUP_READ)
            if (mode and 0b000_010_000 != 0) add(PosixFilePermission.GROUP_WRITE)
            if (mode and 0b000_001_000 != 0) add(PosixFilePermission.GROUP_EXECUTE)
            if (mode and 0b000_000_100 != 0) add(PosixFilePermission.OTHERS_READ)
            if (mode and 0b000_000_010 != 0) add(PosixFilePermission.OTHERS_WRITE)
            if (mode and 0b000_000_001 != 0) add(PosixFilePermission.OTHERS_EXECUTE)
        }
        runCatching { Files.setPosixFilePermissions(f.toPath(), perms) }
            .onFailure { Log.w(TAG, "chmod 失败 ${f.name}: ${it.message}") }
    }

    private fun readName(header: ByteArray): String {
        val name = readString(header, 0, 100)
        val prefix = readString(header, 345, 155)
        return if (prefix.isEmpty()) name else "$prefix/$name"
    }

    /** 路径穿越防护：任何解析结果都必须落在 dest 之内 */
    private fun resolve(dest: File, name: String): File? {
        val cleaned = name.removePrefix("./").trimEnd('/')
        if (cleaned.isEmpty()) return dest
        if (cleaned.startsWith('/')) return null

        val out = File(dest, cleaned)
        val canonical = runCatching { out.canonicalPath }.getOrNull() ?: return null
        val base = runCatching { dest.canonicalPath }.getOrNull() ?: return null
        return if (canonical == base || canonical.startsWith("$base/")) out else null
    }

    private fun readString(b: ByteArray, off: Int, len: Int): String {
        var end = off
        val max = off + len
        while (end < max && b[end] != 0.toByte()) end++
        return String(b, off, end - off, Charsets.UTF_8).trim()
    }

    /** tar 里的数字是八进制，可能以 NUL 或空格结尾 */
    private fun readOctal(b: ByteArray, off: Int, len: Int): Long {
        var v = 0L
        var i = off
        val max = off + len
        while (i < max) {
            val c = b[i].toInt() and 0xFF
            if (c == 0 || c == ' '.code) {
                // 前导空格跳过，遇到终止符就停
                if (v != 0L) break
                i++
                continue
            }
            if (c < '0'.code || c > '7'.code) break
            v = v * 8 + (c - '0'.code)
            i++
        }
        return v
    }

    private fun isGzip(input: InputStream): Boolean {
        input.mark(4)
        val magic = ByteArray(2)
        val n = input.read(magic)
        input.reset()
        return n == 2 && (magic[0].toInt() and 0xFF) == 0x1F && (magic[1].toInt() and 0xFF) == 0x8B
    }

    private fun readFully(input: InputStream, buf: ByteArray): Boolean {
        var read = 0
        while (read < buf.size) {
            val n = input.read(buf, read, buf.size - read)
            if (n < 0) return read == 0
            read += n
        }
        return true
    }

    private fun readData(input: InputStream, size: Long): ByteArray {
        val out = ByteArray(size.toInt())
        readFully(input, out)
        return out
    }

    private fun copyN(input: InputStream, out: java.io.OutputStream, size: Long) {
        val buf = ByteArray(64 * 1024)
        var left = size
        while (left > 0) {
            val n = input.read(buf, 0, minOf(buf.size.toLong(), left).toInt())
            if (n < 0) break
            out.write(buf, 0, n)
            left -= n
        }
    }

    private fun skipData(input: InputStream, size: Long) {
        var left = size
        val buf = ByteArray(8 * 1024)
        while (left > 0) {
            val n = input.read(buf, 0, minOf(buf.size.toLong(), left).toInt())
            if (n < 0) return
            left -= n
        }
    }

    /** 数据区按 512 对齐补齐 */
    private fun skipPadding(input: InputStream, size: Long) {
        val pad = ((BLOCK - (size % BLOCK)) % BLOCK).toInt()
        if (pad > 0) skipData(input, pad.toLong())
    }
}
