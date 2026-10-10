# Linux 沙箱：OpenMinis 是怎么做到的

> 参考实现：`/home/vihv/Code/lib/OpenMinis`（GPL-3.0，公开仓库 OpenMinis/OpenMinis）
> 调研时间：2026-10-10

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
  "name": "linux_run",
  "description": "在设备上的 Alpine Linux 沙箱里执行 shell 命令。可以装包(apk add)、跑 python/node、读写文件。",
  "kind": "NeedsApproval",   // 有副作用
  "schema": { "script": "string" }
}
```

这样 agent 就有了真正的计算机：读界面（`device_snapshot`）+ 操作界面（`device_tap`）
+ **跑代码**（`linux_run`）。这三件加起来才是 OpenMinis 那个形态。

### 决策点

| # | 决策 | 选项 | 倾向 |
|---|---|---|---|
| **D13** | proot 从哪来 | A 复用 `OpenMinis/proot` fork 自编（含 Android 必需补丁）<br>B 上游 `termux/proot` 自编<br>C 先直接用本机已编好的产物跑通 | **C 先跑通 → A 固化**。上游没有 loader/link2symlink 的 Android 处理 |
| **D14** | rootfs 分发 | A 内置 assets（APK +3.9 MB，离线可用）<br>B 首次启动下载 | **A**。4 MB 换离线可用和启动速度，值 |
| **D15** | 接受 GPL 吗 | — | **需要你确认** |
| **D16** | 谁执行 proot | A Rust<br>B Kotlin | **A**，可桌面测试 |
| **D17** | 要不要 `--native-offload` 那种机制 | A 要（把沙箱内的命令名路由回宿主能力）<br>B 不要 | 先不要。我们的设备能力已经走 `DeviceApi` 了，不需要第二条路 |
| **D18** | `--fake-netlink` | A 开<br>B 不开 | **不开**。它是对 guest 撒谎，会让失败点后移 |

### 前置依赖

**我们的 Android 壳还没写。** 目前只有 `core/`（Rust）和 `web/`（Svelte），
`app/` 还是空目录。所以顺序是：

1. **Android 最小壳**：`MainActivity`(WebView) + 加载 core + 权限
   → 这时 D1/D2/D3 三个架构决策才真正在设备上验证
2. **Linux 沙箱**：桌面用 x86_64 proot 开发 `linux_run`，再移植到 arm64
3. **AccessibilityService**：设备操控
