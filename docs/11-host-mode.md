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

不能用 `SandboxConfig` 里挂一个 `host: bool`：那样宿主模式也得带着 `proot` / `loader` /
`dns` / `shared` 这些对它毫无意义的字段，十几处分岔全靠 `if`。proot 专属的方法
（`guest_env`、`proot_command_line`、`prepare_proot`、`resolved_dns`）直接拿
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
  systemd 的服务、用户会话的 daemon 全都是 ppid 1（一次能扫出 40 个：
  journald / NetworkManager / sshd…），而输出里写的是「`agent kill <pid>` 可以
  停掉」。所以多一道闸：**得真的是 proot**（argv[0] 的文件名对得上）才算孤儿，
  它名下的 tracee 照旧收；宿主模式则干脆不扫 /proc，`agent ps` 回一句「用真的
  `ps` / `pgrep`」。回归测试在 `linux.rs` 的
  `procs_attributes_survivors_to_the_command_that_left_them`。
- **`agent` 命令的取数**：`get` / `post` 两个都留（沙箱里只有 busybox 的 `wget`，
  桌面上一般只有 `curl`）。

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
| 孤儿 Chrome 回收 | 扫 `/proc` | 没有 `/proc`（扫不了）；临时 profile 也不在 `/tmp` 而在 `/var/folders/…` | 问 `ps -axww -o pid=,ppid=,command=`；临时 profile 前缀跟着 `temp_dir()` 走 |
| 找局域网里的核心 | `avahi-browse -rp _agent-core._tcp` | `dns-sd` 要两个进程才问得全（`-B` 只给名字，地址和 TXT 还得再 `-L`），没验过的代码不写 | 这一侧没有自动发现：面板里**手输地址**（`desktop::browse` 直接返回 `None`，日志里说一句为什么） |

数据目录仍是 `~/.local/share/android-agent`（`$XDG_DATA_HOME` 优先），**故意没用**
`~/Library/Application Support`：截图路径会原样出现在 `read` / `![](sandbox:…)`
里，路径带空格的话 Markdown 链接直接断。

两个平台共用的顺手修复：

- **设备通道不能拿 mock 顶**：mock 对什么都回成功 —— `agent open` 打出「已让手机
  打开」其实什么都没开，`agent command` 的点击回一句假的 `mock: tapped`。宿主
  模式的设备就是这台电脑（`core/src/desktop.rs` 的 `HostDevice`）：`open` 交给
  `open` / `xdg-open`，做不到的（点、滑、按键）明说做不到。`agent` 脚本里不需要
  任何宿主模式的特判 —— 走的还是同一条 `/api/command`。mock 只留给 `just dev`
  和测试。
- **提示词要如实写宿主能干什么**：桌面上有核心自己起的无头 Chrome，提示词不能说
  「没有 `agent browser`」—— 那样模型会白白放弃浏览器。

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

工具表只有一份（写死了「Alpine 沙箱 / 已预装 / apk add / `agent ps`」）是不行的：
宿主那两份只能在提示词里**整句反驳工具表**（「那几句是手机版的写法，不适用」）——
每轮多花 token，模型同时看着两套互相矛盾的说法。现在工具表本身就是这台机器的。

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
`AGENT_DATA_DIR` 可以换。**只放内存不行**：关一次核心，你在界面上配的模型和所有
会话就没了（`just dev` 那种临时用法除外）。

- `just dev`（proot 沙箱，在电脑上复现手机）**照旧只在内存里**，除非显式给了
  `AGENT_DATA_DIR`。
- `.env.local` / `AGENT_LLM_*` 里的模型只在**库里还没有模型配置**时采用（会存下来，
  跟在界面上配一次一样），不会每次启动都把界面上配好的覆盖掉。
- 访问凭据：这台电脑自己（127.0.0.1）来的不要；局域网来的要（见下一节）。
  设了 `AGENT_TOKEN` 就是本机也要。

## 单实例：这台机器上只跑一个核心

`agent-dev`（`just dev` 和 `just dev-host` 都是它）起来先抢一把 `flock`
（`$XDG_DATA_HOME/android-agent/core.lock`），抢不到就**不起**，把现在跑着的那个
（pid / 端口 / 命令行，是它自己写在锁文件里的）打出来：

```
  已经有核心在跑了：pid 1698717，端口 8787，./core/target/release/agent-dev
  这台机器上只允许一个 —— 两个会静默地互相干扰：mDNS 广播互相挤掉、
  会话库的内存态互相覆盖（两边都不报错，只是行为怪）。先停掉它再起：
    kill 1698717
  确实要同时跑两个（DOM 审计 / 基准测试）：AGENT_MULTI=1
```

为什么是这么做的：

- **用 `flock`，不用 PID 文件**：PID 会复用，进程被 SIGKILL 时文件还留着，得自己判断
  「陈没陈」；文件锁由内核在进程消失时放掉（SIGKILL 也一样），不存在陈旧锁这回事。
- **不问端口，也不看库**：`AGENT_PORT` 一换、库一分开就看起来「互不相干」了，但两个
  核心照样会互相干扰 —— mDNS 里同名服务没法共存（后起的赢，先起的广播静默死掉，
  见上一节的广播检查），两个库各自的内存态也各说各话。所以锁放在**数据目录之外**，
  换端口、换库都挡得住。
- `AGENT_MULTI=1` 是**唯一的出口**，给「故意要第二个」用：DOM 审计
  （`scripts/dom-audit.sh` 自己带上了）和基准测试。它**不碰锁**，所以既不会被挡，
  也不会反过来挡住你自己的核心。

## 局域网：用手机（或另一台电脑）上的界面操控这台电脑

**宿主模式默认就对局域网开放**（听在 `0.0.0.0`）—— 不用另起一个特殊命令：
`just dev-host` 起来，别的机器（手机 App、另一台电脑上的浏览器）侧栏底栏的
「选择机器」就能找到它。只想自己用：

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

| | 只在手机核心上 | 电脑上的核心 |
|---|---|---|
| 听在哪 | 写死 `127.0.0.1` | `AGENT_BIND`（`Core::serve_on`）；手机上照旧只听 loopback |
| 凭据 | 桌面上默认不设防 | **听在非 loopback 上就必须有**：`AGENT_TOKEN`，没给就用库里那把（宿主模式有库，重启不变，收藏的地址一直能用）；本机来的免（对端地址靠 `into_make_service_with_connect_info`） |
| WebSocket 来源 | 只放本机来源 | 另外放**同源**（`Origin` 和 `Host` 对得上）—— 从手机开时来源是 `http://192.168.x.x:8787`；`sandbox.localhost`、`null` 照样挡 |
| AI 写的网页 | 放到 `sandbox.localhost` 这个独立的源上开 | 不是从本机访问时 `sandbox.localhost` 解析到的是**手机自己**，所以回落到同源：服务端给同源的沙箱文件加 CSP sandbox，iframe 也不给 `allow-same-origin` —— 照样碰不到凭据，只是页面用不了 localStorage |

打印的地址是**各网卡上私有网段**（192.168/16、10/8、172.16/12）的 IPv4。不用「往外
连时系统挑哪张网卡」那个办法：开着 Clash 之类 TUN 模式的机器上挑中的是虚拟网卡
（198.18.x.x），手机连不到 —— 这么选过就会打出一个错地址。

### 在界面上切机器（App 和电脑浏览器是同一份）

**自动发现 + 一次性配对**（参照安卓「无线调试」），一套实现两个宿主（见 [docs/12](12-relay.md)）：

- 电脑：`just dev-host` 起来就用系统自带的工具广播 mDNS 服务 `_agent-core._tcp`
  （macOS `dns-sd`、Linux `avahi-publish-service`，`desktop::advertise`），终端里
  打一个 6 位配对码。TXT 记录里只有实例 id 和版本 —— **没有凭据**：自动发现只该
  给出「地址」，把 token 放进广播等于告诉局域网里所有人。用系统工具而不是自己
  实现 mDNS，是因为它们按网卡回答地址（不会把 Clash TUN 的 198.18.x.x 广播出去）。
  - 这个码在**两个地方**看得到：终端启动那几行，和电脑上的 **设置 → 连接手机**
    （`web/src/components/PairingSection.svelte`，接口 `GET /api/pairing`）。
    后者是给后台起的核心用的：`setsid` / 日志重定向进文件时，终端那行就找不着了，
    而后台跑是常态。
  - `/api/pairing` **只回给 loopback**（对端地址）：码的意义就是「证明你人在
    电脑前」，从局域网里也能读到的话这一步就白做了。所以手机上（不管 App 还是
    浏览器）打开设置页，拿到的是 `available: false`，这一栏不显示。
  - **广播到底起没起来，会被确认**（`desktop::Advert::failure`）。这两个工具失败
    时只往 stderr 说一句、退出码还是 0，只看 `spawn` 成不成功是看不出来的：核心
    spawn 完等 `ADVERT_SETTLE`（300ms）再问一句「还在吗」，退了就把那句话原样打
    到终端和日志里 —— `avahi-daemon` 没跑（`创建客户端对象失败：Daemon 不在运行`）、
    或者这个名字已经被另一个核心占了（`添加服务失败：本地名称冲突`），都看得见，
    不会照着「手机上点它」往下打。（这台开发机上就撞过：avahi 一直 disabled、
    广播一直是死的，而终端一直在说「手机上点「jl」」。）
  - 前提是系统的 mDNS 服务真在跑：Linux 要 `systemctl enable --now avahi-daemon`
    （Arch 装完 avahi 默认不 enable，这里就撞过）。同名服务在 mDNS 里没法共存
    —— 先起的那个静默死掉、别的机器上只剩后起的 —— 同一台机器上跑第二个核心就
    给它换个名字：`AGENT_ADVERT_NAME="jl 测试" AGENT_PORT=8789 …`（默认取 `computer_name()`）。
- **找别的核心**：一份发现，两个宿主。
  - 电脑上是核心自己起的 `avahi-browse -rp _agent-core._tcp`（`desktop::browse`，
    跟着核心一直挂着）：一行一行读，出现／解出／消失分别对应面板上多一行／补上地址／
    拿掉。**只认真实网卡的私有 IPv4** —— 同一台机器会在 wlan0 / docker0 / utun / lo
    上各出一行，收错了就得到一个连不上的地址（这台开发机上就有 198.18.0.1 和
    172.17.0.1，见 `usable_iface` / `usable_addr`）。自己不算（按实例 id 剔掉）。
  - 手机上是壳的 `CoreDiscovery`（`NsdManager`）：面板开着时一直找，另外回到前台、
    网络变化时找 15 秒。找到的（配没配对都算）通过 `AgentHost` 之外的**核心接口**报
    进去（`AgentApp.core.lanFound(id, name, host, port)`），关面板时 `lanReset`。
  - 两边报的是同一份东西，核心（`machines.rs`）记在同一张表里 —— 页面上那个面板问的
    就是它（`GET /api/machines`）。
- **配对**：点没配过的那台 → 页面里输 6 位码（手机上那个原生框没了：面板、配对、
  连接现在全在核心里一份代码）→ 核心经 **TLS** `POST /api/pair` 换回 token、记下证书
  指纹，按实例 id 记在库里（`machines` 表；IP 会变，id 不变），接着就把隧道起好、
  页面导航过去。
  - **手机上页面只能点「核心在局域网里真的看到过」的那台**：核心直接拒了带
    `host`/`port` 的配对请求（`machines_pair`），地址一律从「本机自己看到过」的那份里取。
    理由和以前一样：`AgentHost` 每个 frame 都看得见，包括查看器里 AI 写的网页
    （见下）。**电脑上**页面就是本机核心自己发的，多一个「手输地址」的口子
    （广播工具不在、`avahi-daemon` 没跑时还能用）。
  - 配对码输错 5 次就停用到核心重启；`/api/pair` 是唯一不要凭据的 `/api`。
- 电脑不认我们记着的 token 了（换过库）：页面拿到 403，App 那边会说出名字并忘掉它，
  提示重新配对。
- **出门在外**：经自己的服务器当中继连回来（端到端加密），见 docs/12-relay.md。
- **macOS 上找电脑用 `dns-sd`**（`desktop::browse_dnssd`）：一个进程问不全，分三步 ——
  `-B` 常驻看谁上线 / 下线，新看到的一台 `-L` 问端口和 `id`，再 `-G v4` 问局域网 IPv4
  （不走 getaddrinfo：开着 Clash fake-ip 时它会答 198.18.x）。`-Z` 一次给全但**不报下线**，
  关掉的电脑会一直挂在面板上，所以不用它。
- 面板里的电脑**按名字排**（`Machines::panel`），不按先看到的在前 —— 发现结果陆续到、
  一台在局域网 / 经服务器之间换的时候，位置不跳。

连上之后整个 App 就改连那台电脑（页面加载核心给的那个
`http://localhost:<端口>/?k=…`，见 docs/12）；下次启动接着连（是哪台记在核心的库里，
隧道端口尽量复用上次那个 —— 电脑上重启核心后浏览器那个标签刷新一下就能接着用）。
哪条路都不通时隧道回一页 502 写明原因，App 那边自动回本机并说一句为什么。

几处要知道的：

- **为什么页面不需要「能传任意地址」的接口**：换机器时页面只是把核心给它的地址交回给
  壳（`AgentHost.navigate(url)`），壳拿核心算出来的地址（`currentMachineUrl`）对一下
  `host:port`，只认那两个：本机的、和现在这条隧道的。页面里的 JS（包括查看器里 AI 写的
  网页）翻自己的 history、`location`，都拿不到任何一台机器的凭据 —— 隧道转发时替它带上
  （见 docs/12）。
- **App 连电脑不再走明文**：局域网和中继都是手机核心隧道里的 TLS（docs/12）。电脑的同一个
  端口仍然认明文 HTTP —— 给电脑上自己开的浏览器、局域网里用手机浏览器打开的那种。
  （`network_security_config.xml` 放开明文那条还留着：agent 的浏览器要能开 http 网站。）
- 连着电脑时头部的地球按钮（看 agent 的浏览器）不显示：那时 agent 的浏览器在电脑上，
  手机这只跟它无关。
- **连着哪一台，侧栏底栏写着**（最右边那个小标签）：电脑上就是那台电脑的名字
  （`/api/health` 的 `computer`，和 mDNS 广播 / 配对码那栏同一个 `desktop::computer_name`），
  手机上自己的核心写「本机」。同一份界面两种归属，不写一句就只能点进设置才知道。
- **那个标签点开是「选择机器」面板**（`MachineSheet.svelte`，docs/05 的 4.5）：本机 +
  局域网里／经服务器的那几台，点一下就地换。这是唯一的入口。面板在哪个页面上打开都一样：
  它问的那份数据（`GET /api/machines`）**永远是本机核心答的** —— 经隧道时隧道把
  `/api/machines*` 转回本机（见 docs/12）。不这么分的话，连着 A 的时候面板会在 B 的库里
  查「我能连哪些机器」，永远是空的。
- 切换后清掉 WebView 的历史，返回键不会退回另一台的页面。

代价：**用手机浏览器**开电脑上的核心时还是明文 HTTP，`t=` 就是钥匙，Wi-Fi 上能抓包的人能
拿到它（等于拿到这台电脑的 shell）。只在自己信得过的网络里那样用；App 走的是 TLS（docs/12）。
## 该知道的代价（是真的）

- **没有审批环节**（铁律 7）：模型的所有命令直接执行，等于把这台机器交出去。
  要试大改动就用 `just dev`，那里坏了删 rootfs 就行。
- `sandbox:` 链接这时映射的是**整台机器**的文件，只看那把随机的
  `sandbox_key`（同源拆分仍然挡着「AI 写的网页读 App 凭据」那条路）。
- 点击 / 滑动 / 按键只有手机上有（无障碍那条路）；电脑上的设备只会打开东西、起应用。
- 浏览器由**核心自己起**（第一次用到时才拉起无头 Chrome，退出时收掉）——
  不用手动开。想用你自己那个浏览器（能看着它动）就 `AGENT_CDP=…`；
  `AGENT_CDP=off` 就是不要浏览器。
- **线程数封顶，不看 CPU 核数**：走 `#[tokio::main]` 的话默认按核数开 worker
  （32 核的机器就是 33 个线程），而核心里几乎全是等 IO 的活。所以桌面和安卓共用
  `agent_core::runtime()`：4 个 worker + 阻塞池上限 4 + 1（跑 runtime 的那个线程）
  ≤ 10（见 `core/src/lib.rs` 的 `RUNTIME_THREAD_BUDGET`，那一行是编译期断言）。
