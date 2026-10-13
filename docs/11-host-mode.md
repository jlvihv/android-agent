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
是这个类型的**另一种形态**，**没有第二套执行器**：

```rust
enum Mode {
    Proot(Box<SandboxConfig>),       // 安卓（以及 `just dev`）
    Host { workspace: PathBuf },     // 宿主模式：cwd = workspace，rootfs = `/`
}
```

进程模型（runner + 状态文件 + 命令自己的进程组）和路径解析是共用的；随形态
变的地方都 `match` 它，编译器替我们盯着两边都处理到：

1. `command_line()`：宿主模式不给 proot，直接 `[/bin/sh, -c, <runner>, sh, <script>, <status>]`。
2. `run_live()`：cwd 换成 workspace，**继承宿主环境**（`guest_env` 那套干净
   白名单正好相反：在家里电脑上，PATH / 代理 / `CARGO_HOME` 都得是用户自己的）。
   宿主模式不登记命令（`agent ps` 那套靠 proot 归因）。
3. `resolve_path()`：`rootfs()` 是 `/`，于是沙箱内的绝对路径就是本机路径 ——
   `read` / `edit` / `sandbox:` 链接那条路**一行都没改**；
   `/workspace` 仍是 workspace 目录的别名。
4. `prepare_rootfs()` / `is_ready()` / `is_provisioned()` / `has_bash()`：不往 `/`
   里写东西，不跑 `apk`；bash 用 PATH 里的。
5. `layout()`：我们自己的副产物落哪（状态文件、截图、溢出输出）。宿主模式下是
   `~/.local/share/android-agent/{exit,browser}` 和系统临时目录，而不是 rootfs 的
   `/.agent-exit`、`/root/.browser`、`/tmp`（那些是用户的目录）。

以前是 `SandboxConfig` 里一个 `host: bool`：宿主模式也得带着 `proot` / `loader` /
`dns` / `shared` 这些对它毫无意义的字段，十几处分岔全靠 `if`。现在 proot 专属的
方法（`guest_env`、`proot_command_line`、`prepare_proot`、`resolved_dns`）直接拿
`&SandboxConfig` 当参数 —— 类型上就不可能在宿主模式下被调到。

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
- 访问凭据：这台电脑自己（127.0.0.1）来的不要；局域网来的要（见下一节）。
  设了 `AGENT_TOKEN` 就是本机也要。

## 局域网：用手机上的界面操控这台电脑

**宿主模式默认就对局域网开放**（听在 `0.0.0.0`）—— 不用另起一个特殊命令：
`just dev-host` 起来，手机 App 的「连接电脑」就能找到它。只想自己用：

```sh
AGENT_BIND=127.0.0.1 just dev-host
```

（`just dev` 那条沙箱模式是在电脑上复现手机，默认只听本机。）

局域网来的请求都要凭据；**这台电脑自己（loopback）来的照旧免登录**
（`AppState::trust_loopback`，只有桌面会打开 —— 手机上别的 App 也能连
127.0.0.1，那里绝不能开）。所以电脑上自己开浏览器还是 `http://127.0.0.1:8787`。

用手机浏览器也行：启动时会打一行「手机上打开：`http://192.168.x.x:8787/auth?t=…`」，同一个 Wi-Fi 里的
手机用浏览器开它一次（种上 cookie、跳回首页），之后就是同一个界面，只是 `bash` /
`read` / `edit` / `agent browser` 都作用在电脑上。想常用就「添加到主屏幕」。

为了这条路改的四处（都是「只给本机用」的假设）：

| | 原来 | 现在 |
|---|---|---|
| 听在哪 | 写死 `127.0.0.1` | `AGENT_BIND`（`Core::serve_on`）；手机上照旧只听 loopback |
| 凭据 | 桌面上默认不设防 | **听在非 loopback 上就必须有**：`AGENT_TOKEN`，没给就用库里那把（宿主模式有库，重启不变，收藏的地址一直能用）；本机来的免（对端地址靠 `into_make_service_with_connect_info`） |
| WebSocket 来源 | 只放本机来源 | 另外放**同源**（`Origin` 和 `Host` 对得上）—— 从手机开时来源是 `http://192.168.x.x:8787`；`sandbox.localhost`、`null` 照样挡 |
| AI 写的网页 | 放到 `sandbox.localhost` 这个独立的源上开 | 不是从本机访问时 `sandbox.localhost` 解析到的是**手机自己**，所以回落到同源：服务端给同源的沙箱文件加 CSP sandbox，iframe 也不给 `allow-same-origin` —— 照样碰不到凭据，只是页面用不了 localStorage |

打印的地址是**各网卡上私有网段**（192.168/16、10/8、172.16/12）的 IPv4。不用「往外
连时系统挑哪张网卡」那个办法：开着 Clash 之类 TUN 模式的机器上挑中的是虚拟网卡
（198.18.x.x），手机连不到 —— 实测就是这么打出过一个错地址。

### 在 App 里切（不用手机浏览器）

**自动发现 + 一次性配对**（参照安卓「无线调试」）：

- 电脑：`just dev-host` 起来就用系统自带的工具广播 mDNS 服务 `_agent-core._tcp`
  （macOS `dns-sd`、Linux `avahi-publish-service`，`desktop::advertise`），终端里
  打一个 6 位配对码。TXT 记录里只有实例 id 和版本 —— **没有凭据**：自动发现只该
  给出「地址」，把 token 放进广播等于告诉局域网里所有人。用系统工具而不是自己
  实现 mDNS，是因为它们按网卡回答地址（不会把 Clash TUN 的 198.18.x.x 广播出去）。
- 手机：**设置 → 连接电脑** 那一栏**开着时才找**（`CoreDiscovery`，平台的
  `NsdManager`），关掉设置页、App 切到后台都停。找到的电脑列在那儿：
  - 第一次连：点它 → 原生对话框里输配对码 → `POST /api/pair` 换回 token，
    按电脑的实例 id 记在手机上（IP 会变，id 不变）；
  - 配对过的：点它 → 原生确认 → 连上。
  - 页面只能传「找到过的 id」（`connectDiscovered`），给不了任意地址。
- 配对码输错 5 次就停用到核心重启；`/api/pair` 是唯一不要凭据的 `/api`。
- 电脑不认手机记着的 token 了（换过库）：`/auth` 回 403，壳忘掉这把钥匙、提示重新配对。
- 找不到时（广播工具不在、网络隔离多播）还能展开「手动输入地址」。

App 的 **设置 → 连接电脑**（手动那条路）：粘贴上面那行地址 → 连接 → **原生确认框**里点连接，
整个 App 就改连那台电脑（`MainActivity` 的 WebView 改加载它）；同一栏里有「回到
手机」。选择记在壳的 SharedPreferences 里，下次启动直接连；连不上时是原生错误页，
有「重试」和「回到手机」。手机自己的核心一直在后台，回去是瞬间的。

几处要知道的：

- **为什么要原生确认框**：`addJavascriptInterface` 注入的 `AgentHost` 在**每个
  frame** 里都看得见，包括查看器里 AI 写的网页（iframe）。不确认的话那种网页一句
  `AgentHost.connectRemote("http://…")` 就能把 App 带到别人的服务器上；原生对话框
  网页画不出来、也点不了。「回到手机」没有风险，不确认。
- **明文 HTTP 整体放开了**（`network_security_config.xml` 的 `base-config`）：
  局域网地址没法按域名列例外。只影响 Java / WebView 这层（Rust 核心出站走自己的
  rustls）；顺带 agent 的浏览器也能开 http 网站了。
- 连着电脑时头部的地球按钮（看 agent 的浏览器）不显示：那时 agent 的浏览器在电脑上，
  手机这只跟它无关。
- 切换后清掉 WebView 的历史，返回键不会退回另一台的页面。

代价：**明文 HTTP**，`t=` 就是钥匙，Wi-Fi 上能抓包的人能拿到它（等于拿到这台电脑的
shell）。只在自己信得过的网络里用；在外面要连回家里的电脑，走 Tailscale / WireGuard
这类加密隧道，别把端口暴露到公网。

## 该知道的代价（是真的）

- **没有审批环节**（铁律 7）：模型的所有命令直接执行，等于把这台机器交出去。
  要试大改动就用 `just dev`，那里坏了删 rootfs 就行。
- `sandbox:` 链接这时映射的是**整台机器**的文件，只看那把随机的
  `sandbox_key`（同源拆分仍然挡着「AI 写的网页读 App 凭据」那条路）。
- 点击 / 滑动 / 按键只有手机上有（无障碍那条路）；电脑上的设备只会打开东西、起应用。
- 浏览器由**核心自己起**（第一次用到时才拉起无头 Chrome，退出时收掉）——
  不用手动开。想用你自己那个浏览器（能看着它动）就 `AGENT_CDP=…`；
  `AGENT_CDP=off` 就是不要浏览器。
- **线程数封顶，不看 CPU 核数**：桌面以前走 `#[tokio::main]`，默认按核数开 worker
  （32 核的机器就是 33 个线程），而核心里几乎全是等 IO 的活。现在桌面和安卓共用
  `agent_core::runtime()`：4 个 worker + 阻塞池上限 4 + 1（跑 runtime 的那个线程）
  ≤ 10（见 `core/src/lib.rs` 的 `RUNTIME_THREAD_BUDGET`，那一行是编译期断言）。
