# Linux 沙箱：OpenMinis 是怎么做到的

> 参考实现：`/home/vihv/Code/lib/OpenMinis`（GPL-3.0，公开仓库 OpenMinis/OpenMinis）
> 调研时间：2026-10-10
>
> 2026-10-13 复读了 v1.14 快照（`b4c0661`）：他们现在**一条命令一个 proot、
> 不用 `--kill-on-exit`**，超时杀的是命令自己的进程组 —— 见下面「进程模型」。

---

## 0. 为什么这是个难题

我原本以为「把 Alpine rootfs 解压到 `filesDir/`，然后 exec 里面的 `busybox`」就行了。
**这条路在 Android 10+ 上是死的。**

Android 10 起，`targetSdk >= 29` 的应用被 SELinux 强制 **W^X**：

```
# system/sepolicy/private/app_neverallows.te
# Block calling execve() on files in an apps home directory.
# This is a W^X violation (loading executable code from a writable
# home directory).
neverallow untrusted_app app_data_file:file { execute execute_no_trans };
```

`app_data_file` 标签覆盖整个 App 私有目录 —— 包括 `filesDir/alpine-rootfs/`。
所以：

- `chmod +x` 会成功
- `execve("/bin/sh")` 返回 **EACCES**
- 报错信息是误导性的 `Permission denied`，看起来像权限没设对

唯一的例外是 **`nativeLibraryDir`**（`/data/app/<pkg>/lib/arm64/`，标签 `apk_data_file`），
它由 APK 安装器从 `lib/**/*.so` 解出来，**允许执行**。

> 顺带解释了另一个我们之前讨论过的问题：为什么把 Go 编译成可执行文件改名
> `libxxx.so` 丢进 jniLibs 能跑 —— 就是这个原因。

---

## 1. OpenMinis 的解法：两层

### 第一层：proot 本体放 nativeLibraryDir

```kotlin
// RootfsManager.kt:42-43
val rootfsDir   = File(context.filesDir, "alpine-rootfs")
val prootBinary = File(context.applicationInfo.nativeLibraryDir, "libproot.so")
```

`jniLibs/arm64-v8a/libproot.so` —— 一个**可执行文件伪装成 `.so`**。
伪装是必须的：APK 安装器只解压 `lib/**/*.so` 到 nativeLibraryDir，其他名字会被忽略。

### 第二层：ELF loader 也放 nativeLibraryDir（**这才是关键**）

```
jniLibs/arm64-v8a/
├── libproot.so            258 KB  ELF aarch64 PIE，动态链接 /system/bin/linker64
├── libproot-loader.so      18 KB  ELF aarch64，静态链接
└── libproot-loader32.so     6 KB  ELF 32-bit ARM，静态链接
```

proot 的正常工作方式是 fork + `execve` 客体二进制。但在 Android 上
`execve` 一个 `filesDir` 里的二进制必然被拒。

所以 proot fork 被改成：**用 loader 自己 mmap 客体的 ELF 并跳进去，完全不碰内核的 `execve`**。

build 脚本里的原话（`deps/build_proot.sh:11-18`）：

> The loaders are not optional on Android. proot does embed a copy of the loader
> in its own binary and can normally extract it at runtime, but that path is dead
> on Android 10+: the extracted copy lands in the app's temp dir (SELinux label
> `app_data_file`), and W^X forbids `untrusted_app` from executing anything with
> that label — **chmod +x succeeds, the exec still gets EACCES**. The only
> executable location is `nativeLibraryDir`, which the installer populates from
> `lib/**/*.so` inside the APK. Hence the loaders ship as jniLibs with a `.so`
> suffix despite being executables, not shared objects.

以及失败时的症状（很值得记）：

> Drop them and the failure is quiet and misleading: **proot launches fine and
> native_offload initialises, so any "does the sandbox start?" check passes —
> but the first `execve("/bin/sh")` fails and every command in the app comes back
> as `[Shell not running] (exit code: -1)`**.

`PRootKernel.kt` 里还留了 `PROOT_LOADER` / `PROOT_LOADER_32` 环境变量覆盖：

```kotlin
// PRootKernel.kt:142-149
// PROOT_LOADER / PROOT_LOADER_32 overrides. The loader is bundled into the
// proot binary (extracted via /proc/self/fd at runtime), so these files
// normally do not exist — kept only to honour a side-loaded loader if one is present.
val loaderPath = File(rootfsManager.nativeLibDir, "libproot-loader.so")
if (loaderPath.exists()) prootLoaderPath = loaderPath.absolutePath
```

也就是说 fork 里 loader 有**两条路**：memfd/`/proc/self/fd`（不落盘）和
nativeLibraryDir 文件。两条都通，后者是兜底。

---

## 2. 完整布局

### 编译期（APK 内）

```
src/android/app/src/main/
├── jniLibs/arm64-v8a/
│   ├── libproot.so              # proot 可执行文件
│   ├── libproot-loader.so
│   └── libproot-loader32.so
└── assets/
    ├── alpine-minirootfs.tar.gz # 3.85 MB，521 个条目
    └── proot-aarch64            # 258 KB（同一份 proot 的副本）
```

### 运行时（设备上）

```
/data/data/com.openminis.app/
├── files/alpine-rootfs/                  # 解压后的 rootfs（不可执行，靠 loader mmap）
│   ├── .arch                             # 架构标记，用于判断是否要重装
│   ├── bin/ usr/ etc/ root/ ...
│   └── opt/bin/  var/minis/...
└── cache/proot-tmp/                      # proot 的 TMPDIR

/data/app/com.openminis.app-xxx/lib/arm64/   # = nativeLibraryDir
├── libproot.so                           # ← 从这里执行
├── libproot-loader.so
└── libproot-loader32.so
```

`minSdk 26 / targetSdk 35 / compileSdk 36`，**只出 `arm64-v8a`**（`abiFilters`）。

---

## 3. 实际命令行

`PRootKernel.buildProotCommand()`（`PRootKernel.kt:571-650`）：

```
<nativeLibDir>/libproot.so \
  -0 \
  --link2symlink \
  -r <filesDir>/alpine-rootfs \
  -b /dev -b /proc -b /sys \
  -w /root \
  [-b <hostPath>:<linuxPath> ...] \
  [--native-offload=<abstract-socket>:<handler1,handler2,...>] \
  [--fake-netlink] \
  /bin/sh -c "<command>"
```

逐个说：

| 参数 | 为什么 |
|---|---|
| `-0` | 假装 uid 0。`fake_id0` 扩展把 guest 看到的 uid/gid 都改写成 root，否则 `apk` 装包会因为不是 root 而失败 |
| `--link2symlink` | **Android 必需**。Android 的 `/data`（zygote 挂的 ext4/F2FS）拒绝 app uid 跨目录 `link()`。而 Alpine 的 `apk` 装 busybox-applet 包（binutils、gcc 依赖）时全靠硬链接 `ar/ld/nm/strip → busybox`，不加这个参数每次装包都 EPERM |
| `-r <rootfs>` | 设根目录 |
| `-b /dev -b /proc -b /sys` | 绑定伪文件系统 |
| `-w /root` | 工作目录 |
| `-b host:linux` | 用户挂载。他们用 `/var/minis/{soul,skills,shared,attachments,offloads,workspace,browser}` 这些前缀把宿主目录暴露给沙箱 |
| `--native-offload` | **OpenMinis 自己的扩展**。走 abstract unix socket，把沙箱内某个命令名的 `execve` 拦截下来，转交给宿主侧 Kotlin/原生实现（设备集成、浏览器自动化等）。他们在 rootfs 里为每个 handler 放一个 stub 文件，stub 内容无所谓 —— proot 在它跑起来之前就改写了 execve |
| `--fake-netlink` | **OpenMinis 自己的扩展**。`untrusted_app` 不能建 `AF_NETLINK/NETLINK_ROUTE` socket，而 Go 的 `net.Interfaces()` 一启动就调它，报 `route ip+net: netlinkrib: permission denied`，tailscale 这类工具直接起不来。加这个参数后 proot 自己回答 rtnetlink，返回**空接口列表** |

> 他们自己在代码里标注 `--fake-netlink` 是「⚠️ INTERNAL TESTING ONLY」：
> 这是对 guest 程序撒谎 —— 只做枚举的调用方能启动，但要挑源接口或 bind 本地地址的
> 调用方会在**更晚**、更难查的地方失败。

### 环境变量（`bootLocked`，`PRootKernel.kt:150-200`）

```kotlin
PATH        = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/opt/bin"
HOME        = "/root"
ENV         = "/etc/profile"   // ash 非登录 shell 也能读到 profile
CHARSET     = "UTF-8"
NO_COLOR    = "1"
PYTHONDONTWRITEBYTECODE = "1"
GOMAXPROCS  = "2"              // 手机上限
UV_LINK_MODE = "symlink"       // 见下
TZ          = posixTz()        // 必须 POSIX 格式 "LCL-8"
```

两个有信息量的：

- **`TZ` 必须是 POSIX 格式**（`UTC+8` → `LCL-8`，符号是反的）。
  注释原文：`avoid abbreviations like "GMT+8" which contain +/- and confuse musl's TZ parser`。
- **`UV_LINK_MODE=symlink`**：proot 的 `link2symlink` 扩展会在每个硬链接文件旁边留
  `.l2s.*` 哨兵文件；`uv` 默认 `hardlink` 模式会试图把这些哨兵再链接一次，
  proot 返回 EPERM。强制 uv 用符号链接。

---

## 4. 构建链

```
deps/build_proot.sh          → assets/proot-aarch64
                               jniLibs/arm64-v8a/{libproot,libproot-loader,libproot-loader32}.so
deps/prepare_alpine_rootfs.sh → assets/alpine-minirootfs.tar.gz
deps/build_ish.sh            → iOS 用（iSH 模拟 x86）
deps/build_ffmpeg.sh         → ffmpeg
deps/build_lame.sh           → mp3 编码
deps/build_rclone_android.sh → gomobile 绑 rclone 成 .aar
```

要点：

- **NDK r28+**，他们实际用 r28（clang 19）；本机那份产物是 **NDK r27**（clang 18）编的
- **静态链接 `libtalloc`**（`talloc-2.x` 单文件编译，绕开 Samba 的 waf 构建系统）。
  talloc 版本 pin 死
- proot 用的是 fork：`OpenMinis/proot` ← fork 自 `termux/proot` ← fork 自上游 `proot-me/proot`
  - 上游 proot 本身很活跃（**5.5.0，2026-10-02**）
  - termux fork 加了 `link2symlink`、`fake_id0`、`ashmem_memfd`、`kompat` 等 Android 补丁
  - OpenMinis 又加了 `native_offload`、`fake_netlink`
- 产物**不入库**（gitignored），脚本现编
- 可复现性说明：`these artifacts are NOT byte-identical across NDK releases —
  the loader's .text differs between toolchain generations`。功能等价，别指望 checksum 一致
- Gradle：**8.11.1 / AGP 8.7.3 / Kotlin 2.1.0 / JDK 17**，CMake 3.22.1
- 小 JNI 库（`pty_bridge`、crash handler、jieba）由 CMake 在正常 Gradle 构建里编

### rootfs 处理（`RootfsManager.kt`）

- **手写 POSIX tar 解析器**（`extractTar`）。原因：
  - AAPT 会把 `assets/*.tar.gz` 自动解成 `.tar`，所以**两种文件名都要试**
  - 需要进度回调
  - `openFd()` 对 0 长度文件会失败，要按压缩长度/未压缩长度分别处理
- `.arch` 文件记录架构，`isInstalled` 靠它判断
- `default_mount/` 目录**每次启动都覆盖**到 rootfs 上，这样 profile 脚本、
  镜像源配置、URL 拦截 wrapper 能随 App 更新一起更新
- 覆盖之后**再重新应用用户选的镜像源**（否则每次启动都被打回默认）
- `refreshDns()` 从系统刷 `/etc/resolv.conf`

---

## 5. 值得抄的坑清单

| # | 坑 | 症状 | 解法 |
|---|---|---|---|
| 1 | rootfs 里的二进制不能 exec | `execve("/bin/sh"): Permission denied`，但 proot 本身启动正常 | loader 放 `nativeLibraryDir`，mmap 而非 execve |
| 2 | 启动竞态 | 第二个调用者在 loader 路径还是空的时候就起 shell → SELinux 拒 | `bootMutex.withLock` 串行化，锁内**再检查一次** `isBooted` |
| 3 | 硬链接被拒 | `apk add` 任何 busybox-applet 包都 EPERM | `--link2symlink` |
| 4 | rtnetlink 被拒 | Go 程序 `netlinkrib: permission denied` | `--fake-netlink`（有副作用，慎用） |
| 5 | musl 的 TZ 解析器 | 时间不对 | `TZ` 用 POSIX 格式 `LCL-8` 而不是 `GMT+8` |
| 6 | uv 装包 EPERM | `.l2s.*` 哨兵文件被二次 link | `UV_LINK_MODE=symlink` |
| 7 | AAPT 改文件名 | `.tar.gz` 变 `.tar` | 两种都试 |
| 8 | `openFd()` 对 0 长度文件失败 | 解压崩 | 按压缩/未压缩长度分别处理 |
| 9 | 只解压 `lib/**/*.so` | 可执行文件被忽略 | 一律用 `lib*.so` 命名 |

---

## 6. ⚠️ 许可证

| 组件 | 许可证 |
|---|---|
| OpenMinis | **GPL-3.0** |
| `OpenMinis/proot` / `termux/proot` / 上游 proot | **GPL-2.0** |
| iSH | GPL-3.0 |
| Alpine rootfs | 各包各自（大多是 MIT/BSD/GPL 混合） |

**把 proot 二进制捆绑进 APK 并分发 → GPL 传染**（proot 是独立进程，
通常算「聚合」而非「衍生」，但 loader 是被链接进 proot 的，且 fork 本身是 GPL）。
自用、或项目本身开源为 GPL，都没问题；**闭源商业分发需要法务确认**。

---

## 7. 落到我们项目上

### 好消息：可以在桌面先跑通

proot 是个普通 Linux 程序，x86_64 版本随便装。所以 `linux_run` 工具、
rootfs 管理、命令构造这些逻辑**全部可以在桌面上开发验证**，
跟我们前面「同源 localhost + mock 设备」的路子完全一致。

只有 loader / nativeLibraryDir / SELinux 那部分必须真机验证。

### 架构落点

```
Kotlin（薄壳）
  └─ 只在 boot 时把两个路径交给 Rust：
       nativeLibDir   → proot 二进制所在
       filesDir       → rootfs 所在
                              │
Rust core                     ▼
  └─ Command::LinuxRun { script }
       → std::process::Command 拼 proot 命令行
       → 捕获 stdout/stderr/exit code
       → 作为 tool result 回给模型
```

**为什么放 Rust 而不是 Kotlin**：可以桌面测试，且和现有的
`DeviceApi` / `Command` 抽象一致。Kotlin 侧只需要多传两个路径字符串。

### 新工具

```jsonc
{
  "name": "bash",
  "description": "在设备上的 Alpine Linux 沙箱里执行 shell 命令。可以装包(apk add)、跑 python/node、读写文件。",
  // timeout 可选，默认 60s；长任务模型显式给（上限 24h）。超时被杀时结果里
  // 会提醒它下次给足（见 agent.rs 的 timeout_hint）。超时 exit_code = 124。
  "schema": { "command": "string", "timeout": "number (seconds, optional, default 60)" }
}
```

（早期设计里工具分 `ReadOnly` / `NeedsApproval` 两类，后者要等用户点批准。
审批环节后来整个删了：工具一律直接执行，`ToolKind` 也随之消失。）

沙箱里还预置了一个 `agent` 命令（`prepare_rootfs` 写进 `/usr/local/bin/agent`）：
它是核心自己那个 HTTP 服务的薄包装，让模型能查看自己（`agent state` /
`agent request` / `agent tools`）、操作这台手机（`agent command`）、看/停沙箱里
还在跑的进程（`agent ps` / `agent kill <pid>`，见上面「进程模型」）。
见 [docs/07-debugging.md](07-debugging.md) §8。

### 进程模型：一条命令一个 proot、一个进程组

**一条命令一个 proot**（`run_live()`），不用长命 shell 复用。OpenMinis 量过
这件事并把数字写在了 `ShellExecutionStrategy.kt` 里：他们的 guest 里
`sh -c "echo hi"` 冷启动 4.2 ms（`setsid` 版 8.5 ms），而每次工具调用的宿主
开销是 65–90 ms —— 复用 shell 省下的那点钱，换来的是整套带内完成标记的坑
（标记被 `read()` 切两半、修它的 hold-back 缓冲把所有短输出吞了、超时命令
继续污染下一条命令的 stdout，五个月炸三次）。我们要的只是进程退出码。

**没有 `--kill-on-exit`**（曾经有，已删）。它让 proot 在命令退出的瞬间对整棵
树发 SIGKILL（proot 的 `terminate_tracee` → `kill_all_tracees`），于是
`nohup` / `setsid` 起的常驻服务**一条也活不过这次调用** —— 而「起一个后台
服务」是这个工具该有的能力。删掉它之后 proot 会一直等到所有 tracee 退出
（它得给每条 tracee 做路径翻译），所以：

- **完成信号变成状态文件。** 命令跑在一层 runner 里（`runner_script()`，命令行
  的最后一个参数）：
  `setsid /bin/sh -c '<runner>' sh <script> <status>`。runner 在命令退出后把
  **命令的**退出码写进 `<rootfs>/.agent-exit/<uuid>`，宿主以它为准（读不到才
  退回 proot 的退出码）。不这样的话，一条 `setsid … &` 起的常驻进程会让 proot
  继续活着 —— 它就是那根 stdout 管道的写端，宿主永远等不到 EOF。
- **杀的是命令的进程组，不是 proot**。`setsid` 让 runner 当组长，runner 把
  `$$` 写进 `<status>.pid`；超时 / 用户按停止时 `kill(-pgid, SIGTERM→SIGKILL)`
  （`CommandGuard`）。**不能杀 proot**：它可能正在给上一条命令留下的常驻进程
  当 tracer，proot 一死那些进程就失去 rootfs 翻译（看到的路径全变成宿主机的），
  比直接杀掉更糟。宿主侧 `process_group(0)` 是兜底目标（rootfs 里没有 `setsid`，
  或者 runner 还没写下 pid 就被杀）。
- **不再有孤儿。** 原来超时 / 停止走 `child.start_kill()` / `kill_on_drop`，
  都是 SIGKILL **proot 本身**；而 proot 的 `--kill-on-exit` 是 userspace 的
  `atexit` 钩子，SIGKILL 不跑它 —— 被 trace 的整棵 guest 树就留在那里继续吃
  CPU。现在 future 被丢掉时 `CommandGuard::drop` 杀的是进程组。
- **超时 `exit_code = 124`**（跟 `timeout(1)` 一致），命令被信号打死时按 shell
  的 128+N 约定给出 `signal`（于是「被 OOM 杀了」这类提示还能给）。

> **判 `setsid` 在不在不能用 `is_file()`。** Alpine 里 `/usr/bin/setsid` 是指向
> `/bin/busybox` 的**绝对**符号链接，在宿主上解析会指向宿主机的 `/bin/busybox`
> （不存在）。真机上就这么漏过一次：命令行里没有 setsid，runner 落在 proot 自己
> 的进程组里，超时那一杀打到 proot 头上（它忽略 SIGTERM，然后吃 SIGKILL），
> 它 trace 的常驻进程全部变成没翻译的孤儿 —— 看起来“命令被杀干净了”，实际是
> 把整个沙箱的翻译层一起打掉了。改用 `symlink_metadata` 判存在，
> `is_alpine_rootfs()` 上早写过同样的教训。

用户视角的规则（工具描述里就是这么写的）：

| 写法 | 这次调用正常结束后 | 这次调用超时 / 被停止时 |
|---|---|---|
| `cmd &`（裸的） | 活着 | 连它一起杀 |
| `setsid cmd &` / `nohup cmd &` | 活着 | 活着（刻意脱离，不动它） |
| `cmd`（前台） | 跟着命令结束 | 杀掉 |

判据在内核（进程组），不是对命令文本找 `&` —— `&` 在 `curl 'a?x=1&y=2'`、
`grep 'a & b'`、heredoc 里都是普通文本，正则判不出来（`ForegroundCommandGroup.kt`
里那段论证很到位，我们是拿同样的道理直接用了进程组）。

**事后怎么收场**：`agent ps` 列出「命令结束后还活着的常驻进程」（带 pid / pgid /
属于哪条命令），`agent kill <pid>` 停掉（先 SIGTERM，几秒不走再 SIGKILL）。
归因靠 `TracerPid` —— guest 进程的 tracer 就是那条命令的 proot，而**只有核心
记得**哪个 proot 是哪条命令起的（刻意脱离的进程父进程早就是 init 了，光靠 `ps`
认不出它们是谁、什么时候起的）。实现在 `linux.rs` 的 `LinuxSandbox::procs`
（`RunRegistry` + `/proc` 扫描）；`/proc` 被 hidepid 限制的机器上它会**明说**
「看不见」，而不是假装「什么都没有」。对应的接口是 `GET /api/procs`。

> 代价得说清楚：**App 进程被系统杀掉时 proot 一起死**，它 trace 的常驻进程
> 虽然还在跑但已经失去 rootfs 翻译（等于坏了）；App 被冻住时（见
> [docs/06](06-persistence-and-background.md)）整棵树也跟着停。所以「常驻」只
> 承诺活过工具调用，不承诺活过 App。真要「以后再做」，得靠调度而不是后台进程。

### 预装的工具（`agent-setup`）

minirootfs 里**只有 busybox**：没有 python、没有 curl、没有 jq，`grep` 也不认 `-P`。
这些几乎每个任务都要碰到，所以 App 启动后会后台跑一次 `agent-setup`
（`prepare_rootfs` 写进 `/usr/local/bin/agent-setup`，清单在 `linux.rs` 的 `TOOLSET`）：

```
bash curl ca-certificates grep findutils jq git github-cli diffutils patch ripgrep fd tree unzip python3 openssl
```

三个实现上的坑：

- **不能拿 `apk add` 的退出码当准**。proot 下 busybox / ca-certificates 的 trigger
  常常返回非零 —— 实测装一个全新的小包也会报 `1 error; N packages`，但包确实装上了。
  所以脚本以「那些命令找不找得到」为准（`missing()`），装完再写标记文件。
- **标记文件里存清单版本**（工具数）。只判文件在不在的话，以后往 `TOOLSET` 里加工具，
  老设备会永远以为自己装过了。
- **安卓上 `serve()` 比 rootfs 解压早**，所以 `provision_tools()` 先等 `is_ready()`
  （用 `is_alpine_rootfs` 判断），最多等 3 分钟。装过程走 `st.log`，在 App 的事件页
  能看到「正在给沙箱装常用工具…」。

装好之后，`bash` 工具**默认用 bash 执行**（`command_line()` 里判 `bin/bash` 在不在，
不在就退回 `/bin/sh`）—— 模型写的 `[[ ]]`、数组、进程替换、`<<<` 才能跑。
`PAGER` / `GIT_PAGER` 固定成 `cat`：分页器在非交互环境会**挂住**，一次工具调用就废了。

这样 agent 就有了真正的计算机：**跑代码**（`bash`）+ 沙箱里的文件系统，
这才是 OpenMinis 那个形态。

### 沙箱里没有浏览器，但手机上有：`agent-open`

登录类命令（`gh auth login --web`、`python -m webbrowser`、`xdg-open`）原本在沙箱里
全是 `not found` —— 沙箱里没有浏览器，可用户手里这台手机有。

`prepare_rootfs` 会写 `/usr/local/bin/agent-open`，以及 `xdg-open` /
`sensible-browser` / `www-browser` / `x-www-browser` / `gnome-open` / `kde-open`
六个转发垫片；`guest_env()` 把 `BROWSER` **强制**指过去（覆盖用户 dotfile 里的值，
跟 OpenMinis `PRootKernel.kt:159` 的 T195 一个道理）。

它**真的会打开**：`agent-open` 只是三行壳，逻辑在 `agent open <uri>` 里 ——
`POST /api/command`（`{"cmd":{"type":"open","url":…}}`）→ 设备通道 `Command::Open`
→ Kotlin `UriLauncher`（`ACTION_VIEW`；`intent://` 走 `Intent.parseUri` 还原
action/extras，没 App 接时退到 `S.browser_fallback_url`）。`http(s)` 找浏览器或
对应的 App，`weixin://` / `androidamap://` 这类深链直接进 App 的具体页面（能带
参数），`tel:` / `mailto:` / `geo:` / `market:` 各归各家。

> 为什么 `agent-open` 这个小壳还在：`$BROWSER` 和那六个名字都得是**一个可执行
> 文件**，写不了 `agent open`（不同调用方拆参数的方式还不一样，python 的
> webbrowser 还会拿 `%s` 去替换）。逻辑只留一份，在 `agent open` 里。

**不做 scheme 白名单**，跟 `agent command` 那条路一个立场：这是 agent 自己的手机，
工具调用一律直接执行、没有审批环节；真打不开（没装那个 App、系统没有 handler）
由系统报错回来，agent 看得到。用户点聊天里的链接走的是同一个 `UriLauncher`
（那条路本来就是用户的明确动作）。

> `launch`（按包名起 App，只能到首页）和 `open`（按 URI 到具体页面/动作）是两件事，
> 都留在设备通道里：前者不需要知道对方的深链格式，后者能带参数。

**打开 URI 不需要无障碍**。设备通道（`DeviceHostImpl`）是无障碍服务连上才注进来的；
没开无障碍时，`dispatch` 把 `open` 交给浏览器那条（`BrowserController` 的内部动作
`__open_uri`，它启动时就注册了，不依赖无障碍）。两条路的差别只在后台：无障碍服务的
context 豁免「后台不许起 Activity」，App 自己的不豁免 —— 所以没开无障碍、App 又在
后台时，`__open_uri` 先自己检查前台状态并**明说打不开**（系统拦的时候不抛异常，
不查的话 agent 会以为已经开了）。`__` 开头的动作只在壳和核心之间用，模型这边
（沙箱里的 `agent browser`）不接受。

需要用户在**别的** App 里完成的事（典型是 `gh auth login --web`）：页面 `agent-open`
会自己开，但**一次性码得 agent 在回复里告诉用户**；这种命令还要等用户操作，所以
得放后台（`setsid gh auth login --web > /tmp/x.log 2>&1 &`，见上面「进程模型」——
`setsid` 起的能活过这次调用），之后再查日志收尾 —— 前台等会死锁（用户在码出现
在聊天里之前根本看不到它）。

另外输出里有网址时，bash 结果里会带一个 `urls: [...]`（没有就不带这个字段）（`urls` 模块扫输出里的 http(s) 网址）：
gh 的设备码页是它自己 print 出来的（不是所有 CLI 都会去调浏览器），给模型一份去重、
修过标点的清单，它拿这些去 `agent-open` 或在回复里给用户都不容易抄错。

> 聊天里的链接原本点了没反应：`<a target="_blank">` 在 WebView 里算弹窗，而
> `setSupportMultipleWindows` 默认 false、又没实现 `onCreateWindow` ——
> browser 的 WebView 是靠注入 JS 把 `_blank` 掰成 `_self` 绕过去的，聊天这个没那套。
> 现在 `MainActivity` 实现 `onCreateWindow`，和点链接一起走 `UriLauncher`。

> 早期的设计把读界面 / 操作界面做成 `device_snapshot` / `device_tap` 两个工具。
> 现在它们不作为工具暴露给模型了 —— 以后收进沙箱里的一个命令、由 `linux_run`
> （现名 `bash`）调用，这样模型只需要理解 shell，工具表也只有两个。

### 决策点

| # | 决策 | 选项 | 倾向 |
|---|---|---|---|
| **D13** | proot 从哪来 | A 复用 `OpenMinis/proot` fork 自编（含 Android 必需补丁）<br>B 上游 `termux/proot` 自编<br>C 先直接用本机已编好的产物跑通 | **C 先跑通 → A 固化**。上游没有 loader/link2symlink 的 Android 处理 |
| **D14** | rootfs 分发 | A 内置 assets（APK +3.9 MB，离线可用）<br>B 首次启动下载 | **A**。4 MB 换离线可用和启动速度，值 |
| **D15** | 接受 GPL 吗 | — | **需要你确认** |
| **D16** | 谁执行 proot | A Rust<br>B Kotlin | **A**，可桌面测试 |
| **D17** | 要不要 `--native-offload` 那种机制 | A 要（把沙箱内的命令名路由回宿主能力）<br>B 不要 | 先不要。我们的设备能力已经走 `DeviceApi` 了，不需要第二条路 |
| **D18** | `--fake-netlink` | A 开<br>B 不开 | **不开**。它是对 guest 撒谎，会让失败点后移 |
| **D19** | 进程模型 | A 保留 `--kill-on-exit`：命令一退整棵树 SIGKILL（简单，但后台常驻服务全死，而且我们杀 proot 时它根本不跑 → 漏孤儿）<br>B 命令跑在自己的进程组里 + 状态文件当完成信号（后台常驻可用；超时/停止按组杀，proot 留着） | **B**（已实现）。见上面「进程模型」。代价是 completion 不再等于 proot 退出，多一个 runner + 状态文件 |

### 前置依赖

**我们的 Android 壳还没写。** 目前只有 `core/`（Rust）和 `web/`（Svelte），
`app/` 还是空目录。所以顺序是：

1. **Android 最小壳**：`MainActivity`(WebView) + 加载 core + 权限
   → 这时 D1/D2/D3 三个架构决策才真正在设备上验证
2. **Linux 沙箱**：桌面用 x86_64 proot 开发 `linux_run`，再移植到 arm64
3. **AccessibilityService**：设备操控
