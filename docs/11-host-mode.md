# 宿主模式：桌面上不套沙箱，直接用这台机器

> 落地日期：2026-10-13

一句话：`just dev-host` 起核心，浏览器开 <http://127.0.0.1:8787>，
`bash` / `read` / `edit` 三个工具作用在**这台 Linux 本身** —— 没有 proot、
没有 Alpine rootfs、没有 `/workspace` 那层映射。

| | `just dev`（沙箱） | `just dev-host`（宿主） |
|---|---|---|
| `bash` 跑在哪 | proot 里的 Alpine | 本机（`/bin/sh -c <runner>` → 宿主的 bash） |
| cwd | rootfs 的 `/workspace` | 启动核心时的目录（也以 `/workspace` 可见） |
| `read` / `edit` 的路径 | rootfs 里的路径 | 本机路径（rootfs＝`/`） |
| 工具 | Alpine 那套（要 `apk add`） | 你的 PATH（node/cargo/python…都在） |
| 装东西 | `apk add` / `agent-setup` | 系统包管理器 / venv |
| 系统提示词 | `SYSTEM_PROMPT`（手机那份） | `SYSTEM_PROMPT_HOST` |
| 进程模型 | runner + 状态文件 + `CommandGuard` | **同一套**（只是不套 proot） |
| `agent ps` | 列常驻进程（靠 proot 的 `TracerPid` 归因） | 只回一句「用真的 `ps` / `pgrep`」 |
| `agent kill <pid>` | ✅ | ✅ |
| `agent command`（设备） | mock | mock（手机上才是真的） |
| `agent browser` | ❌（要手机上的 WebView） | ❌（还没接桌面后端） |
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

## 系统提示词：两份常量，不是插值

`SYSTEM_PROMPT` 必须是常量（缓存按前缀命中，见 [docs/03](03-prompt-cache.md)），
所以宿主模式**不是**在提示词里插「你现在在 `/home/xxx`」，而是另有一份常量
`SYSTEM_PROMPT_HOST`（`core/src/agent.rs`），启动时按沙箱形态选一份
（`AppState::builtin_prompt`）。

两份只在「在哪跑、有哪些门」上不同，工具表和其余规则一字不差 ——
不然两边干活的方式会慢慢漂。宿主那份还必须点名一件事：**工具表里仍然写着
「Alpine Linux 沙箱」「已预装 …」「apk add」「`agent ps`」**（工具表是缓存前缀，
没跟着分叉），所以提示词里把这几句挑明成「那是手机版的写法，宿主模式按本机
理解；后台服务同样用 `setsid … &`，收场用 `ps` / `pgrep` / `kill`」。
不加这句的实测后果：模型会去 `apk add`，白花一轮。

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

## 该知道的代价（是真的）

- **没有审批环节**（铁律 7）：模型的所有命令直接执行，等于把这台机器交出去。
  要试大改动就用 `just dev`，那里坏了删 rootfs 就行。
- `sandbox:` 链接这时映射的是**整台机器**的文件，只看那把随机的
  `sandbox_key`（同源拆分仍然挡着「AI 写的网页读 App 凭据」那条路）。
- `agent browser` / 设备操作在桌面上还没有实现，模型只能 `curl` 取网页。
  （要的是「agent 用我自己那个浏览器」的话，得在核心侧接一个 CDP 后端 ——
  就是 `state::BrowserApi` 的第三个实现，和 Kotlin 的 WebView 那个并列。）
