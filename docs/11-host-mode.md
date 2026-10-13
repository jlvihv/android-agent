# 宿主模式：桌面上不套沙箱，直接用这台机器

> 落地日期：2026-10-13

一句话：`just dev-host` 起核心，浏览器开 <http://127.0.0.1:8787>，
`bash` / `read` / `edit` 三个工具作用在**这台 Linux / Mac 本身** —— 没有 proot、
没有 Alpine rootfs、没有 `/workspace` 那层映射。

| | `just dev`（沙箱） | `just dev-host`（宿主） |
|---|---|---|
| `bash` 跑在哪 | proot 里的 Alpine | 本机（`/bin/sh -c <runner>` → 宿主的 bash） |
| cwd | rootfs 的 `/workspace` | 启动核心时的目录（`/workspace` 只是工具路径里的别名，shell 里没有） |
| `read` / `edit` 的路径 | rootfs 里的路径 | 本机路径（rootfs＝`/`） |
| 工具 | Alpine 那套（要 `apk add`） | 你的 PATH（node/cargo/python…都在） |
| 装东西 | `apk add` / `agent-setup` | 系统包管理器 / venv |
| 系统提示词 / 工具表 | `Platform::Android` 那份 | `Platform::Linux` / `Platform::Macos` 那份（见下面「按平台拼」） |
| 进程模型 | runner + 状态文件 + `CommandGuard` | **同一套**（只是不套 proot） |
| `agent ps` | 列常驻进程（靠 proot 的 `TracerPid` 归因） | 只回一句「用真的 `ps` / `pgrep`」 |
| `agent kill <pid>` | ✅ | ✅ |
| 设备（`agent open` / `agent command`） | mock | **这台电脑**（`desktop::HostDevice`）：`open` 用默认程序打开，macOS 上 `launch` = `open -a`；点 / 滑 / 按键明说没有 |
| `agent browser` | ✅ 核心自己起的无头 Chrome + CDP（[docs/08](08-browser.md) 第 15 节） | ✅ 同一个后端 |
| `agent search` | ✅ | ✅ |

## 怎么做到的：只加了「一种沙箱形态」，没加第二条路

三个工具全都走 `LinuxSandbox`（`core/src/linux.rs`）同一个入口，所以宿主模式
就是给这个类型加了一种配置，**没有第二套执行器**：

```rust
SandboxConfig { host: true, rootfs: "/", workspace: Some(<cwd>), .. }
```

打开之后各处的差别只有五处：

1. `command_line()`：不给 proot，直接 `[/bin/sh, -c, <runner>, sh, <script>, <status>]`。
2. `run_live()`：cwd 换成 workspace，**继承宿主环境**（`guest_env` 那套干净
   白名单正好相反：在家里电脑上，PATH / 代理 / `CARGO_HOME` 都得是用户自己的），
   并开一个独立进程组。
3. `resolve_path()`：`rootfs` 是 `/`，于是沙箱内的绝对路径就是本机路径 ——
   `read` / `edit` / `sandbox:` 链接那条路**一行都没改**；
   `/workspace` 仍是 workspace 目录的别名。
4. `prepare_rootfs()` / `is_ready()` / `is_provisioned()`：不往 `/` 里写东西，
   不跑 `apk`。`is_provisioned()` 直接返回 true —— 否则启动时会去装那套工具。
5. `exit_dir()`：状态文件落在 `$XDG_DATA_HOME/android-agent/exit`，
   而不是 rootfs 的 `/.agent-exit`（那是用户的根目录）。

`SandboxConfig` 里那些安卓专属项（`loader`、`dns`、`shared`）在宿主模式下不参与。

### 进程模型是**共用**的，不是另写一套

上游那套「命令自带进程组 + 状态文件当完成信号」（见 [docs/02](02-linux-sandbox.md)
的「进程模型」）在宿主模式同样成立，而且**更需要**：`setsid <命令> &` 起的常驻
服务会一直拿着 stdout，靠「管道 EOF」当完成信号的话，一条 `setsid python3 -m
http.server &` 就得等到超时（默认 60s）才回来。所以宿主模式照样跑 runner、照样
写状态文件、照样用 `CommandGuard` 收尾 —— 只是命令行里没有 proot 那一串，
状态文件落在我们自己的数据目录里。

### 顺手修掉的两个「安卓才成立」的假设

- **`agent ps` 不能只看 `ppid == 1`。** 上游那套孤儿识别（「父进程没了 = 上一次
  App 生命留下的」）在安卓上没问题（/proc 按 uid 过滤），在桌面上是灾难：
  systemd 的服务、用户会话的 daemon 全都是 ppid 1 —— 实测一次列出 40 个
  （journald / NetworkManager / sshd…），而输出里写的是「`agent kill <pid>` 可以
  停掉」。现在多一道闸：**得真的是 proot**（argv[0] 的文件名对得上）才算孤儿，
  它名下的 tracee 照旧收；宿主模式则干脆不扫 /proc，`agent ps` 回一句「用真的
  `ps` / `pgrep`」。回归测试在 `linux.rs` 的
  `procs_attributes_survivors_to_the_command_that_left_them`。
- **`agent` 命令的取数**：那个脚本原来只认 busybox 的 `wget`，桌面上一般只有
  `curl`，所以 `get` / `post` 现在两条都留（装完 curl 的沙箱也会走 curl）。

## macOS

> 落地日期：2026-10-13

macOS 上**只有宿主模式**（proot 是 Linux 专属的，Mac 上也不要沙箱）：
`just dev-host`，其它都一样。核心本来就只用 POSIX 那一套（进程组、`kill(-pgid)`、
状态文件），能直接编过、测试全绿；真正要补的是「Linux 才成立」的几处假设：

| 地方 | Linux 上的做法 | macOS 上不成立的原因 | 现在 |
|---|---|---|---|
| `setsid` | util-linux 自带 | macOS 没有 `setsid(1)`；退到 `nohup` 不换进程组，超时会被连组杀掉 | 宿主 bin 目录里垫一个（`SETSID_SHIM`：系统 perl 调 `POSIX::setsid()`，组长先 fork，跟 util-linux 一致）。PATH 里有真的就不垫 |
| bash | `/bin/bash` | `/bin/bash` 是 3.2（没有关联数组 / `${x,,}` / `mapfile`） | 用 **PATH 里第一个** bash（装了 Homebrew bash 就是它），找不到才退回 `/bin/bash` |
| 系统提示词 / 工具表 | `ss -ltnp` / `pgrep -af` / `xdg-open` | 这三样 macOS 上都不对 | `Platform::Macos`：`lsof -nP -iTCP -sTCP:LISTEN` / `pgrep -fl` / `agent open`（→ `open`），并点名 BSD 用户态（`sed -i ''` 等） |
| Chrome 在哪 | PATH / `/opt/google/chrome` | `.app` 包从不在 PATH 里 | 查 `/Applications`（和 `~/Applications`）下的 Chrome / Chromium / Canary / Edge / Brave |
| Chrome 钥匙串 | — | 新 profile 存 cookie 时会弹「Chrome Safe Storage」授权框，无头那边一直等 | 加 `--use-mock-keychain` |
| 孤儿 Chrome 回收 | 扫 `/proc` | 没有 `/proc`（原来静默跳过）；临时 profile 也不在 `/tmp` 而在 `/var/folders/…` | 问 `ps -axww -o pid=,ppid=,command=`；临时 profile 前缀跟着 `temp_dir()` 走 |

数据目录仍是 `~/.local/share/android-agent`（`$XDG_DATA_HOME` 优先），**故意没用**
`~/Library/Application Support`：截图路径会原样出现在 `read` / `![](sandbox:…)`
里，路径带空格的话 Markdown 链接直接断。

两个平台共用的顺手修复：

- **设备通道在桌面上原来是 mock，对什么都回成功**：`agent open` 打出「已让手机
  打开」其实什么都没开，`agent command` 的点击回一句假的 `mock: tapped`。现在宿主
  模式的设备就是这台电脑（`core/src/desktop.rs` 的 `HostDevice`）：`open` 交给
  `open` / `xdg-open`，做不到的（点、滑、按键）明说做不到。`agent` 脚本里不需要
  任何宿主模式的特判 —— 走的还是同一条 `/api/command`。mock 只留给 `just dev`
  和测试。
- **宿主提示词原来说「没有 `agent browser`」**，而桌面上早就有核心自己起的无头
  Chrome —— 模型会白白放弃浏览器。改成如实写。

## 系统提示词和工具表：按平台拼，不是抄几份

前缀缓存要求系统提示词和工具表**在同一个部署里字节不变**（[docs/03](03-prompt-cache.md)），
所以宿主模式**不是**在提示词里插「你现在在 `/home/xxx`」。但「不插运行时数据」
不等于「只能写字面量」：

- `Platform`（`core/src/platform.rs`）：`Android` / `Linux` / `Macos`。
  `AppState::platform()` 看沙箱形态定：宿主模式是这个二进制的桌面系统
  （`Platform::host()`，按编译目标），proot 沙箱或还没有沙箱都按手机算。
- `agent::Phrasing`：提示词和 `bash` 工具描述里**随平台变**的那几句（你是谁、
  cwd 规则、设备那一门、浏览器背后是谁、这台机器上有什么）。三份是三个
  `const`，Linux / macOS 共用 `HOST` 再各盖几个字段。
- `compose_prompt` / `bash_description`：共用的句子**只写一份**，按平台把片段
  填进去；`system_prompt(p)` / `tools(p)` 每个平台拼一次就冻结在 `OnceLock` 里。
- 「每个系统的事实」（看进程用 `pgrep -af` 还是 `pgrep -fl`、有没有 `ss`）在
  `Platform::proc_tools()` / `kill_tool()`，提示词、工具表、`agent ps` 的回话
  三处都从这里取。

以前是三份整段字面量，工具表只有一份（写死了「Alpine 沙箱 / 已预装 / apk add /
`agent ps`」）。宿主那两份只好**整句反驳工具表**（「那几句是手机版的写法，不适用」）——
每轮多花 token，模型同时看着两套互相矛盾的说法。现在工具表本身就是这台机器的，
那句反驳删掉了。

安卓那份重构前后**逐字节一致**（`cargo run -q --example dump_prompt` 前后 diff 过），
老会话的缓存不受影响；宿主那两份变了，`ensure_system_prompt` 会把新版本追加进老会话。
测试 `every_platform_gets_a_frozen_prompt_that_matches_its_machine` 盯着：每个平台
拿到的是同一串字节、没有机器相关的路径、宿主模式里没有一个手机上的词。

## 用起来

```sh
just dev-host                                # cwd = 这个仓库（just 会把 cwd 设到 justfile 那儿）
AGENT_HOST_WORKSPACE="$PWD" just dev-host    # 换成你当前那个目录
AGENT_HOST_WORKSPACE=~/work just dev-host    # 或者给绝对路径
```

（`AGENT_LINUX_HOST=1` 是开关；不用 `just` 的话自己 `export` 它就行。）

其它都跟 `just dev` 一样：`just web-dev`（Vite 热更）、
`python3 scripts/agentctl.py "…"`、`--doctor` 里会显示
「宿主模式（无沙箱）」。

`agent` 命令装在 `$XDG_DATA_HOME/android-agent/bin`（默认
`~/.local/share/android-agent/bin`），只在我们发出去的命令的 PATH 前缀里 ——
不动用户的 shell 配置，也不抢他的 `xdg-open`。

## 会话和模型配置落盘

宿主模式是拿来**日常用**的，所以会话、模型配置、附件都落盘：默认
`~/.local/share/android-agent/data/sessions.db`（`$XDG_DATA_HOME` 优先），
`AGENT_DATA_DIR` 可以换。以前桌面上一律只放内存 —— 关一次核心，你在界面上
配的模型和所有会话就没了（在 macOS 上实测时撞到的）。

- `just dev`（proot 沙箱，在电脑上复现手机）**照旧只在内存里**，除非显式给了
  `AGENT_DATA_DIR`。
- `.env.local` / `AGENT_LLM_*` 里的模型只在**库里还没有模型配置**时采用（会存下来，
  跟在界面上配一次一样），不会每次启动都把界面上配好的覆盖掉。
- 访问凭据照旧：桌面上默认不设防，设了 `AGENT_TOKEN` 才要（库里那把随机 token
  在桌面上不生效）。

## 该知道的代价（是真的）

- **没有审批环节**（铁律 7）：模型的所有命令直接执行，等于把这台机器交出去。
  要试大改动就用 `just dev`，那里坏了删 rootfs 就行。
- `sandbox:` 链接这时映射的是**整台机器**的文件，只看那把随机的
  `sandbox_key`（同源拆分仍然挡着「AI 写的网页读 App 凭据」那条路）。
- 点击 / 滑动 / 按键只有手机上有（无障碍那条路）；电脑上的设备只会打开东西、起应用。
- 浏览器由**核心自己起**（第一次用到时才拉起无头 Chrome，退出时收掉）——
  不用手动开。想用你自己那个浏览器（能看着它动）就 `AGENT_CDP=…`；
  `AGENT_CDP=off` 就是不要浏览器。
