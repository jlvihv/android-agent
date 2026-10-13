//! Linux 沙箱：在设备上跑一个真正的 Alpine Linux userland。
//!
//! 参考实现见 `docs/02-linux-sandbox.md`（OpenMinis 的做法）。
//!
//! ## 核心约束
//!
//! Android 10+ 对 `untrusted_app` 强制 W^X：`app_data_file` 标签下的一切
//! （包括 `filesDir/alpine-rootfs/` 里的整个 rootfs）**不能被执行**。
//! `chmod +x` 会成功，`execve` 仍然 EACCES。
//!
//! 所以：
//! - **proot 二进制本身**放在 `nativeLibraryDir`（由 APK 的 `lib/**/*.so`
//!   解出来，标签 `apk_data_file`，可执行）。文件名必须是 `lib*.so`，
//!   否则 APK 安装器根本不会解压它。
//! - **rootfs 里的二进制**由 proot 的 ELF loader 自己 mmap 进内存，
//!   不走内核 `execve`。loader 同样在 `nativeLibraryDir`。
//!
//! ## 为什么这段逻辑在 Rust 而不是 Kotlin
//!
//! 因为 proot 是个普通 Linux 程序 —— 桌面上装个 x86_64 版就能把
//! 命令行构造、超时、输出截断这些逻辑全部测通，不必等真机。
//! Kotlin 只需要在 boot 时把两个路径交进来。

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::editor::{self, Edit, EditOutcome};
use crate::reader::{self, ReadView};

/// 单次命令的默认超时。
///
/// 只给 `bash` 工具之外的入口用（`/api/linux` 的终端/CLI）—— 那边没人替我们
/// 估时间，得给个默认值。**`bash` 工具不走这个默认值**：模型必须自己传 `timeout`
///（见 `agent.rs`），免得它不假思索地依赖一个偏短的默认值，把长时间下载/编译
/// 在半路杀掉。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// 单次命令的超时上限：24 小时。
///
/// 基本等于「不设限」，但总得有个兜底 —— 一条真正挂死的命令不能把沙箱占住到
/// 天荒地老。下载、编译、`apk add` 这类活儿几十分钟到几小时都正常，6000 秒
/// 那种量级完全该允许。
pub const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 3600);

/// `read` 工具允许整份读进内存的文本文件上限。
///
/// 读取要按行定位 `offset`，所以是整份读进来再切 —— 一个 200MB 的日志会把
/// 手机内存吃爆（OOM kill 不是 panic，但一样是崩溃）。超过就回一条错误，
/// 让模型改用 `sed -n` / `head` / `rg` 局部读。
const MAX_TEXT_BYTES: u64 = 32 * 1024 * 1024;

/// 回给模型的输出上限：**超过 `MAX_OUTPUT_LINES` 行或 `MAX_OUTPUT_BYTES` 字节**
/// 才算截断。
///
/// 截断后的呈现是首尾各留 `HEAD_LINES` / `TAIL_LINES` 行、中间用
/// `[省略 N 行]` 顶掉：头是命令的开场（版本号、表头、第一条结果），尾是结论
/// 和报错 —— 两头都有用，所以不像 `read` 那样只看头（bash 的输出没地方续读，
/// 只能靠 `/tmp` 里那份完整输出）。字节预算由两侧平分，保证两边都能露脸。
///
/// 触发线故意留得很宽（2000 行 / 50KB）：1999 行的文件应该一次就看全，
/// 否则模型得反复 offset 续读，比多给几千行贵得多。
pub const MAX_OUTPUT_LINES: usize = 2000;
pub const MAX_OUTPUT_BYTES: usize = 50 * 1024;
/// 截断时首/尾各留多少行
pub const HEAD_LINES: usize = 100;
pub const TAIL_LINES: usize = 100;
/// 截断时分给首/尾各一侧的字节预算（给 `[省略 N 行]` 和换行留点余量）
const SIDE_BYTES: usize = (MAX_OUTPUT_BYTES - 256) / 2;

/// Alpine 的 PATH。
///
/// **必须显式设置。** 宿主（无论 Arch 还是 Android）的 PATH 里通常没有 `/bin`，
/// 而 Alpine minirootfs 的 busybox applet 全在 `/bin` 下 —— 不设的话
/// `ls`、`cat`、`uname` 全部 "not found"，只有 `/bin/sh` 能跑。
const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/opt/bin";

/// 沙箱里固定的工作目录。命令的 cwd、相对路径的基准都是它。
///
/// 安卓上挂的是手机内部存储的 `Agent/` 文件夹 —— 用户在文件管理器里能
/// 直接看到 AI 的产出，App 卸载也不会丢。没有存储权限时（或桌面上）
/// 退回 rootfs 里的同名目录。
pub const WORKSPACE: &str = "/workspace";

/// 手机内部存储整个挂在这里（`/storage/emulated/0`）
pub const SHARED: &str = "/sdcard";

/// `read` 工具看到的东西：图片（缩放后内联）或文本（含截断提示）。
pub enum ReadResult {
    Text(ReadView),
    Image(crate::imaging::Encoded),
}

/// `edit` 工具的 create 模式建出来的文件。
///
/// 只回「建好了、多少行 / 字节」：内容是模型自己刚写进来的，原样回显纯属浪费
/// 上下文（`edit` 回 diff 是因为它没写过改动后的全文）。
#[derive(Debug)]
pub struct CreatedFile {
    pub lines: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// proot 可执行文件。Android 上是 `<nativeLibraryDir>/libproot.so`
    pub proot: PathBuf,
    /// rootfs 根目录
    pub rootfs: PathBuf,
    /// 额外的绑定挂载，`(宿主路径, 沙箱内路径)`
    #[serde(default)]
    pub binds: Vec<(String, String)>,
    /// proot 自己的临时目录（**宿主路径**）。
    ///
    /// 走 `PROOT_TMP_DIR` 环境变量，**不是** `TMPDIR` ——
    /// proot 只在 `PROOT_TMP_DIR` 里找，找不到就退回 `P_tmpdir`（`/tmp`），
    /// 而 Android 上 App 写不了 `/tmp`。
    #[serde(default)]
    pub tmp: Option<PathBuf>,
    /// 64 位 ELF loader，走 `PROOT_LOADER`。
    ///
    /// **安卓上必须设。** 不设的话 proot 会调 `extract_loader()` 把内嵌的
    /// loader 解到临时目录，而那是个 `app_data_file` —— W^X 直接拒绝执行，
    /// 最后报 `'/bin/sh' is not executable`。
    /// 只有 `nativeLibraryDir` 里的文件才允许执行。
    #[serde(default)]
    pub loader: Option<PathBuf>,
    /// 32 位 ELF loader，走 `PROOT_LOADER_32`
    #[serde(default)]
    pub loader32: Option<PathBuf>,
    /// 写进 rootfs 的 `/etc/resolv.conf`。
    ///
    /// **Alpine minirootfs 里根本没有这个文件**，所以 `apk add` 会因为
    /// DNS 解析失败而报 “no such package” —— 看起来像软件源挂了，
    /// 实际是名字都解析不出来。桌面上从宿主机拷；安卓上由 Kotlin
    /// 从 ConnectivityManager 拿系统 DNS 传进来。
    #[serde(default)]
    pub dns: Vec<String>,
    /// 时区偏移（分钟）。安卓上由 Kotlin 从系统时区传进来 ——
    /// 沙箱里读不到宿主时区，只能靠外部注入。
    #[serde(default)]
    pub tz_offset_minutes: Option<i32>,
    /// 额外环境变量
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 挂到 `/workspace` 的宿主目录。None = 用 rootfs 里的 `/workspace`
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    /// 挂到 `/sdcard` 的宿主目录（手机内部存储）。None = 不挂
    #[serde(default)]
    pub shared: Option<PathBuf>,
}

impl SandboxConfig {
    /// 接上（或断开）手机存储：整个存储挂 `/sdcard`，其中的 `Agent/` 挂
    /// `/workspace`。建不了 `Agent/`（没权限）就都不挂，退回 rootfs。
    pub fn with_shared_storage(mut self, storage: Option<&Path>) -> Self {
        self.shared = None;
        self.workspace = None;
        if let Some(root) = storage {
            let ws = root.join("Agent");
            match std::fs::create_dir_all(&ws) {
                Ok(()) => {
                    self.shared = Some(root.to_path_buf());
                    self.workspace = Some(ws);
                }
                Err(e) => tracing::warn!("建不了 {}（没有存储权限？）: {e}", ws.display()),
            }
        }
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub truncated: bool,
    pub timed_out: bool,
    /// 输出被截断时，完整输出落在沙箱内的这个路径上，模型可以再去读/ grep
    pub full_output_path: Option<String>,
    /// 这次用的是不是真 bash。false = 没装 bash，退回了 `/bin/sh`（busybox
    /// ash），此时 `to_text` 会明说一句并让模型去装 —— 否则它写 bash 语法
    /// 只会拿到一句莫名其妙的 `syntax error`。
    pub using_bash: bool,
    /// 被信号杀掉时的信号号（正常退出是 `None`）。没有它的话，`exit -1`
    /// 分不清「被 OOM 杀了」和「proot 自己崩了」。
    pub signal: Option<i32>,
}

impl ExecResult {
    /// 给模型看的紧凑文本
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        if !self.stdout.is_empty() {
            s.push_str(&self.stdout);
        }
        if !self.stderr.is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str("--- stderr ---\n");
            s.push_str(&self.stderr);
        }
        if self.truncated {
            s.push_str("\n[输出已截断]");
            if let Some(p) = &self.full_output_path {
                s.push_str(&format!("\n[完整输出在 {p}]"));
            }
        }
        if self.timed_out {
            // 说清「下面是半截的」，否则模型会当成完整输出，再按它推理
            s.push_str("\n[超时，已终止：下面是到被终止为止的输出，可能不全]");
        } else if let Some(sig) = self.signal {
            s.push_str(&format!("\n[被信号 {sig} 终止：{}]", signal_hint(sig)));
        }
        // 没 bash 就直说，并给出装它的命令。藏在提示词里没用 —— 模型只会在
        // 碰到 `syntax error` 时一头雾水。
        if !self.using_bash {
            s.push_str(
                "\n[沙箱里还没有 bash：这条是 /bin/sh（busybox ash）跑的，[[ ]] / 数组 / \
                 <<< / 进程替换这类 bash 语法会报 syntax error。装上：agent-setup --force]",
            );
        }
        if s.is_empty() {
            s.push_str("(无输出)");
        }
        s.push_str(&format!("\n[exit {}]", self.code));
        s
    }
}

/// 信号号只是个数，模型要的是「它为什么死了」。
fn signal_hint(sig: i32) -> &'static str {
    match sig {
        6 => "abort，多半是断言/内存越界",
        9 => "SIGKILL，多半是被 OOM 杀了",
        11 => "SIGSEGV，段错误",
        13 => "SIGPIPE，管道对面先退出了（比如 head 已读完）",
        15 => "SIGTERM，被要求退出",
        _ => "未知信号",
    }
}

pub struct LinuxSandbox {
    cfg: SandboxConfig,
    /// 核心 HTTP 服务的端口。`agent` 命令靠它找到「自己」（见 `AGENT_CLI`）。
    /// 0 = 还不知道（核心还没 bind），这时不注入环境变量。
    agent_port: AtomicU16,
    /// 核心接口的凭据（`AGENT_TOKEN`）。见 `state::Auth`。
    agent_token: std::sync::RwLock<Option<String>>,
}

/// 沙箱里的 `agent` 命令。
///
/// 为什么要有它：模型平时只看得见自己的上下文和历史，看不见「自己现在什么状态」。
/// 这个命令让它能查自己（状态 / 下一次会收到的完整上下文 / 会话），
/// 也能操作自己所在的这台手机（坐标点击、按键、启动应用）。
///
/// 只用 busybox 的 wget，没有别的依赖 —— Alpine minirootfs 里就有。
/// 端口由 App 通过 `AGENT_PORT` 注入（`guest_env`）。
const AGENT_CLI: &str = r##"#!/bin/sh
# agent —— 在沙箱里和**自己**说话。
#
# 这个 App 的核心是个只听 127.0.0.1 的 HTTP 服务，这个脚本只是它的薄包装：
# 让你能看见自己（状态、下一次会收到的上下文、会话），也能操作自己所在的
# 这台手机。输出基本都是原始 JSON。
#
#   agent help               看全部
#   agent state              我现在的状态
#   agent request            下一次会发给我自己的完整请求体（提示词 + 工具表 + 历史）
#   agent tools              我有哪些工具
#   agent sessions           会话列表
#   agent session            当前这条会话的消息（不会切走你正在看的那条）
#   agent doctor             各部件体检
#   agent setup [--force|--list|包…]   装/看常用工具（rg fd jq git python3…）
#   agent command <json>     操作这台手机，例如 '{"type":"key","name":"back"}'
#   agent open <uri>         让手机打开它：http(s) / App 深链（weixin://、
#                            androidamap://）/ intent:// / tel: …
#                            （$BROWSER / xdg-open 最后也是走到它）
#   agent browser <动作> [参数…]  驱动内置浏览器（真 WebView）：
#       navigate <url> | get_backbone [深度] | click <选择器> | type <选择器> <文本>
#       get_text [选择器] | get_readable | scroll [up|down] [像素]
#       screenshot [true=整页] | press Enter | find_elements <选择器> | user_agent mobile|desktop
#       back | wait_for_dom_stable [秒] | js '<代码>'
#       new_tab [url] | list_tabs | switch_tab <编号> | close_tab [编号]
#     输出是核心给的原始 JSON。一条命令里可以连做多步（这是它比工具调用强的地方）：
#       agent browser navigate <url> && agent browser get_backbone 3 && agent browser click '#x'
#     截图落在 /root/.browser/，要 read 它才看得见（screenshot 的返回里会提醒）。
#     其他参数直接给 JSON：agent browser '{"action":"click","x":100,"y":200}'
#   agent search <检索意图> [条数]   联网搜索（Exa），拿标题 + 链接 + 片段
#   agent get <path>         直接 GET 自己的接口（逃生口）
#   agent post <path> <json> 直接 POST
#
# 端口从 $AGENT_PORT 来。看不到它就说明不是在 agent 的 bash 里。
set -eu

BASE="${AGENT_BASE:-http://127.0.0.1:${AGENT_PORT:-}}"
if [ -z "$BASE" ] || [ "$BASE" = "http://127.0.0.1:" ]; then
  echo "AGENT_PORT 没设 —— 不是在 agent 的 bash 里跑？" >&2
  exit 1
fi
# 核心的接口要凭据（手机上别的 App 也能连 127.0.0.1）。App 通过环境变量给。
AUTH="x-agent-token: ${AGENT_TOKEN:-}"

# 「改自己身份」的接口不从这儿走：别让网页里的一段话把 agent 自己改了。
guard() {
  case "$1" in
    /api/llm*|/api/debug/system-prompt*)
      echo "拒绝：$1 会改掉我自己的身份（模型 / key / 提示词）。" >&2
      echo "那两件事留给人做：在宿主机上跑 scripts/agentctl.py" >&2
      exit 3
      ;;
  esac
}

get() {
  guard "$1"
  wget -q -T 120 -O - --header "$AUTH" "$BASE$1" || { echo "GET $1 失败" >&2; exit 1; }
}

post() {
  guard "$1"
  wget -q -T 120 -O - --header "$AUTH" --header 'content-type: application/json' --post-data "$2" "$BASE$1" \
    || { echo "POST $1 失败" >&2; exit 1; }
}

# 把一行 JSON 摊开，方便人看（没有 jq，纯 shell）
fmt() { tr ',' '\n' | tr -d '{}"' | sed 's/^[[:space:]]*//; /^[[:space:]]*$/d'; }

# 参数里的反斜杠和引号要先转义，否则拼出来的 JSON 是坏的（browser / search 都用）
esc() { printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'; }

cmd="${1:-help}"
case "$cmd" in
  state)    get /api/debug/state | fmt ;;
  request)  get /api/debug/request | fmt ;;
  tools)    get /api/debug/tools | tr ',' '\n' | sed -n 's/.*"name":"\([a-z_]*\)".*/\1/p' ;;
  sessions) get /api/sessions | fmt ;;
  session)  get /api/session | fmt ;;
  doctor)
    get /api/health | fmt
    echo '--- 在沙箱里跑一句 ---'
    post /api/linux '{"command":"echo ok"}'
    ;;
  setup)
    shift || true
    exec /usr/local/bin/agent-setup "$@"
    ;;
  command)
    if [ $# -lt 2 ]; then
      cat >&2 <<'EOT'
用法：agent command '{"type":"key","name":"back"}'
可用的 type：
  {"type":"key","name":"back|home|recents|notifications"}
  {"type":"tap_xy","x":540,"y":1200}
  {"type":"swipe","x1":540,"y1":1800,"x2":540,"y2":600,"ms":300}
  {"type":"launch","package":"com.android.settings"}  只到 App 首页
  {"type":"open","url":"androidamap://navi?…"}         按 URI 到具体页面/动作
EOT
      exit 2
    fi
    post /api/command "{\"cmd\":$2}"
    ;;
  open)
    shift || true
    uri=""
    for a in "$@"; do
      # 任何 `scheme:` 开头都接（tel: 这种没有 //；--flags 以 - 开头不会被误认）
      case "$a" in [a-zA-Z]*:*) uri="$a"; break ;; esac
    done
    if [ -z "$uri" ]; then
      # 不报错：这个命令也是 $BROWSER / xdg-open 的落点，
      # 而它们常被链在脚本里（set -e 下报错会把调用方炸掉）
      echo "agent open: 参数里没有 URI，跳过: $*" >&2
      exit 0
    fi
    out=$(post /api/command "{\"cmd\":{\"type\":\"open\",\"url\":\"$(esc "$uri")\"}}")
    case "$out" in
      *'"ok":true'*)
        echo "已让手机打开: $uri"
        ;;
      *)
        detail=$(printf '%s' "$out" | sed -n 's/.*"detail":"\([^"]*\)".*/\1/p')
        echo "没能打开（${detail:-没有响应}）：$uri" >&2
        ;;
    esac
    ;;
  get)  [ $# -ge 2 ] || { echo "用法：agent get /api/…" >&2; exit 2; }; get "$2" ;;
  post) [ $# -ge 3 ] || { echo "用法：agent post /api/… '{…}'" >&2; exit 2; }; post "$2" "$3" ;;
  browser)
    shift || true
    act="${1:-}"; shift || true
    usage() {
      cat >&2 <<'EOT'
用法：agent browser <动作> [参数…]
  navigate <url>                   打开网页
  get_backbone [深度]              读页面结构（含可点的 selector）
  click <选择器>                    点击（也可以用 JSON 给 x/y）
  type <选择器> <文本>              往输入框里填
  get_text [选择器]                 读整页/某元素的文本（带导航页脚；文章用 get_readable）
  get_readable                     抽正文（文章页用这个）
  scroll [up|down] [像素]           滚动
  screenshot [true]                截图（true = 整页）；存 /root/.browser/，用 read 看
  press <键名>                      按键：Enter（会提交表单）/Tab/Escape/方向键…
  find_elements <选择器>            列出匹配到的元素（个数 + 坐标，坐标可直接给 click）
  new_tab [url]                    开一只新标签（不给 url 就是空白页）
  list_tabs                        标签列表（含当前是哪一只）
  switch_tab <编号>                 切到某只标签
  close_tab [编号]                  关掉某只（不给编号 = 关当前这只）
  user_agent mobile|desktop        切 UA + 视口（会重载页面）
  back                             后退
  wait_for_dom_stable [秒]         等动态内容安定（默认 10）
  js '<代码>'                      在页面里跑一段 JS（可以 await）

一条命令里可以连做多步（省轮次）：
  agent browser navigate <url> && agent browser get_backbone 3 && agent browser click '#x'
参数给不全 / 给多了都会报错，不会静默忽略 —— 少给一个 url 就等于开空白页。
EOT
    }
    # 纯数字参数（标签编号 / 超时秒数）：非数字直接报错，别拼出一段坏 JSON 给核心
    int_arg() { case "${1:-}" in ''|*[!0-9]*) echo "agent browser $2：要一个数字，给的是「${1:-}」" >&2; exit 2 ;; esac; }
    case "$act" in
      help|-h|--help) usage; exit 0 ;;
      "")             usage; exit 2 ;;
    esac

    # 每个动作最多收几个参数，多给就报错。**静默扔掉参数**是这套 CLI 最坑人的错法
    # （踩过：`agent browser new_tab URL` 把 URL 吃了、开出一只空白标签，模型以为
    # 页面开了，对着白屏截了三张图 —— 见 docs/08-browser.md）。
    max=1
    case "$act" in
      get_readable|back|list_tabs|tabs) max=0 ;;
      type|scroll)                      max=2 ;;
    esac
    [ $# -le "$max" ] || { echo "agent browser ${act}：最多 $max 个参数，给了 $# 个" >&2; usage; exit 2; }

    # 参数长得复杂（要 x/y、要多个字段）就直接给 JSON：agent browser '{"action":…}'
    case "$act" in
      navigate)
        [ -n "${1:-}" ] || { usage; exit 2; }
        body="{\"action\":\"navigate\",\"url\":\"$(esc "$1")\"}" ;;
      get_backbone)
        if [ -n "${1:-}" ]; then int_arg "$1" get_backbone; body="{\"action\":\"get_backbone\",\"max_depth\":$1}"; else body='{"action":"get_backbone"}'; fi ;;
      click)
        [ -n "${1:-}" ] || { usage; exit 2; }
        body="{\"action\":\"click\",\"selector\":\"$(esc "$1")\"}" ;;
      type)
        [ -n "${1:-}" ] && [ -n "${2:-}" ] || { usage; exit 2; }
        body="{\"action\":\"type\",\"selector\":\"$(esc "$1")\",\"text\":\"$(esc "$2")\"}" ;;
      get_text)
        if [ -n "${1:-}" ]; then body="{\"action\":\"get_text\",\"selector\":\"$(esc "$1")\"}"; else body='{"action":"get_text"}'; fi ;;
      scroll)
        if [ -n "${2:-}" ]; then int_arg "$2" scroll; body="{\"action\":\"scroll\",\"direction\":\"${1:-down}\",\"amount\":$2}"; else body="{\"action\":\"scroll\",\"direction\":\"${1:-down}\"}"; fi ;;
      screenshot)
        case "${1:-}" in
          ''|false) body='{"action":"screenshot"}' ;;
          true)     body='{"action":"screenshot","full_page":true}' ;;
          *)        echo "用法：agent browser screenshot [true]（true = 整页）" >&2; exit 2 ;;
        esac ;;
      back)         body='{"action":"back"}' ;;
      get_readable) body='{"action":"get_readable"}' ;;
      press)
        [ -n "${1:-}" ] || { usage; exit 2; }
        body="{\"action\":\"press\",\"key\":\"$(esc "$1")\"}" ;;
      find_elements|find)
        [ -n "${1:-}" ] || { usage; exit 2; }
        body="{\"action\":\"find_elements\",\"selector\":\"$(esc "$1")\"}" ;;
      user_agent|ua)
        case "${1:-}" in mobile|desktop) ;; *) echo "用法：agent browser user_agent mobile|desktop" >&2; exit 2 ;; esac
        body="{\"action\":\"set_user_agent\",\"user_agent\":\"$(esc "$1")\"}" ;;
      wait_for_dom_stable)
        if [ -n "${1:-}" ]; then int_arg "$1" wait_for_dom_stable; body="{\"action\":\"wait_for_dom_stable\",\"timeout\":$1}"; else body='{"action":"wait_for_dom_stable"}'; fi ;;
      js)
        [ -n "${1:-}" ] || { usage; exit 2; }
        body="{\"action\":\"js\",\"script\":\"$(esc "$1")\"}" ;;
      new_tab)
        if [ -n "${1:-}" ]; then body="{\"action\":\"new_tab\",\"url\":\"$(esc "$1")\"}"; else body='{"action":"new_tab"}'; fi ;;
      list_tabs|tabs) body='{"action":"list_tabs"}' ;;
      switch_tab)
        int_arg "${1:-}" switch_tab
        body="{\"action\":\"switch_tab\",\"tab\":$1}" ;;
      close_tab)
        if [ -n "${1:-}" ]; then int_arg "$1" close_tab; body="{\"action\":\"close_tab\",\"tab\":$1}"; else body='{"action":"close_tab"}'; fi ;;
      # 第一眼像 JSON 就原样发（复杂参数走这条）
      \{*) [ $# -eq 0 ] || { echo "JSON 形式后面不该再跟参数：$*" >&2; exit 2; }; body="$act" ;;
      # 光杆动作名：核心自己会认（认不出它会回「未知动作」，比在这边猜好）
      *)   body="{\"action\":\"$(esc "$act")\"}" ;;
    esac
    post /api/browser "$body"
    ;;
  search)
    shift || true
    if [ -z "${1:-}" ]; then
      echo "用法：agent search <检索意图> [条数]" >&2
      exit 2
    fi
    q=$(esc "$1")
    if [ -n "${2:-}" ]; then body="{\"query\":\"$q\",\"num_results\":$2}"; else body="{\"query\":\"$q\"}"; fi
    post /api/websearch "$body"
    ;;
  help|-h|--help)
    # 从第 2 行打到「端口从…」那行为止（那行是表头的结尾）。用哨兵行而不是
    # 写死行号：以前是 '2,31p'，表头一改 help 就悄悄少印或多印几行。
    sed -n 's/^# \{0,1\}//p' "$0" | sed -n '2,/端口从/p'
    ;;
  *)
    # ⚠️ 变量后面紧跟中文标点时**必须**写 ${var}：沙箱里是 busybox ash，它不认
    # UTF-8，会把全角字节当成变量名的一部分 —— 于是那个变量变成了一个不存在的
    # 名字，set -u 下直接「unbound variable」退出 1，提示语永远打不出来。
    echo "不认识的子命令：${cmd}（agent help）" >&2
    exit 2
    ;;
esac
"##;

/// `$BROWSER` / `xdg-open` 的落点：转给 `agent open`（逻辑都在那边）。
///
/// 为什么留这个小壳而不是直接把 `BROWSER` 指到 `agent`：`$BROWSER` 和那几个
/// 名字都得是**一个可执行文件**，写不了 `agent open`（不同调用方拆参数的方式
/// 还不一样；python 的 webbrowser 会拿 `%s` 去替换）。
const AGENT_OPEN: &str = r##"#!/bin/sh
# agent-open —— 见 `agent open`。也会以 xdg-open / sensible-browser /
# www-browser / x-www-browser / gnome-open / kde-open 的身份被调用。
exec /usr/local/bin/agent open "$@"
"##;

/// 这六个名字都是“开浏览器”的常见入口（OpenMinis 垫的是同一组），全转发给
/// `agent-open`。
const BROWSER_SHIM: &str =
    "#!/bin/sh\n# 见 agent-open；这些名字都是“开浏览器”的常见入口。\nexec /usr/local/bin/agent-open \"$@\"\n";

/// 预装的工具。**改这里就够了** —— `agent-setup` 脚本是从它生成的。
///
/// 为什么预装：Alpine minirootfs 里只有 busybox（没有 python、没有 curl、
/// 没有 jq、`grep` 不认 `-P`），而模型干活几乎每次都碰它们。装一次，
/// 后面每个回合就不用自己 `apk add`，也不会因为工具的小脾气卡住。
///
/// 增删请顺手想一下体积：手机上这是要下载的（约 75MB），别把 ffmpeg 塞进来。
/// 清单里最大的一块是 `github-cli`（单它装完就 44 MiB）。
pub const TOOLSET: &[&str] = &[
    "bash",            // 提示词里让模型用 bash，沙箱里原来只有 ash
    "curl",            // 比 busybox wget 好用得多
    "ca-certificates", // 没有它 curl/git 走 https 会验证失败
    "grep",            // GNU grep：busybox 的不认 -P
    "findutils",       // GNU find
    "jq",              // JSON（模型处理 JSON 太频繁了）
    "git",
    "github-cli", // gh：不用手掰 API / 开网页就能看 issue/PR、克隆、下 release
    "diffutils",  // diff/cmp：改文件前后对比、自己生成 patch
    "patch",      // 应用 diff（模型写/接补丁时用得上）
    "ripgrep",    // rg
    "fd",
    "tree",
    "unzip", // busybox 没有
    "python3",
    "openssl", // 证书/小工具用得上
];

/// `agent-setup`：装上面那套（幂等）。
/// `agent-setup`：装上面那套（幂等）。
///
/// 注意：**不能拿 `apk add` 的退出码当准**。proot 下 busybox / ca-certificates
/// 的 trigger 常常返回非零（实测：装一个全新的小包也会报 “1 error; N packages”，
/// 但包确实装上了）。所以脚本以**那些命令找不找得到**为准。
const AGENT_SETUP_TEMPLATE: &str = r##"#!/bin/sh
# agent-setup —— 把干活要用的工具装上。
#
# Alpine minirootfs 里只有 busybox（没有 python、没有 curl、没有 jq，
# grep 也不认 -P），而模型几乎每个任务都要碰到它们。App 启动后会自动跑一次
# 这个脚本；也可以随时手动跑（幂等）。
#
#   agent-setup            装标准那一套（装过就秒退）
#   agent-setup --force    不管标记，重跑一遍
#   agent-setup --list     只看看装了哪些
#   agent-setup <包…>      在标准那套之外再加几个
set -eu

MARK=/etc/agent-tools
VERSION=__VERSION__
BASE="__PACKAGES__"
# 装完必须能找到这些命令（ca-certificates 没有命令，单独查证书包）
CHECK="bash curl grep find jq git gh rg fd tree unzip python3 openssl diff patch"
CA=/etc/ssl/certs/ca-certificates.crt

missing() {
  out=""
  for b in $CHECK; do
    command -v "$b" >/dev/null 2>&1 || out="$out $b"
  done
  [ -s "$CA" ] || out="$out ca-certificates"
  echo "$out"
}

if [ "${1:-}" = "--list" ]; then
  for b in $CHECK; do
    printf '%-12s %s\n' "$b" "$(command -v "$b" 2>/dev/null || echo '❌ 没有')"
  done
  printf '%-12s %s\n' "ca-bundle" "$([ -s "$CA" ] && echo "$CA" || echo '❌ 没有')"
  exit 0
fi

force=""
if [ "${1:-}" = "--force" ]; then force=1; shift; fi

if [ -z "$force" ] && [ -f "$MARK" ] && [ "$(cat "$MARK")" = "$VERSION" ] \
   && [ -z "$(missing)" ]; then
  echo "工具就绪（$MARK=$VERSION）。重装：agent-setup --force"
  exit 0
fi

echo "装工具：$BASE $*"
echo "（首次要下几十 MB，之后不再装）"
# apk 的退出码在这个沙箱里不可信：proot 下 busybox/ca-certificates 的 trigger
# 常报 “1 error”，但包确实装上了。所以这里不看它，后面用「命令找不找得到」判断。
apk add --no-cache $BASE "$@" || true

left=$(missing)
if [ -n "$left" ]; then
  echo "还缺：$left" >&2
  echo "多半是网络/DNS。查一下：curl -sI https://dl-cdn.alpinelinux.org" >&2
  echo "然后重跑：agent-setup --force" >&2
  exit 1
fi

mkdir -p /etc
echo "$VERSION" > "$MARK"
echo "好了（apk 那句 1 error 是 proot 下 trigger 的事，以这份清单为准）："
for b in $CHECK; do
  printf '  %-10s %s\n' "$b" "$(command -v "$b" 2>/dev/null || echo '?')"
done
"##;

/// 预装脚本的内容。清单变了这里就变，`write_if_changed` 会重写文件。
fn setup_script() -> String {
    AGENT_SETUP_TEMPLATE
        .replace("__VERSION__", &TOOLSET.len().to_string())
        .replace("__PACKAGES__", &TOOLSET.join(" "))
}

/// 只在内容变了才写，避免每次启动都碰文件（和 resolv.conf 一样的做法）。
///
/// `exec` = 写成 0755（可执行）。
fn write_if_changed(path: &Path, body: &str, exec: bool) -> anyhow::Result<()> {
    let same = std::fs::read_to_string(path)
        .map(|old| old == body)
        .unwrap_or(false);
    if same {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, body)?;
    #[cfg(unix)]
    if exec {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = exec;
    Ok(())
}

impl LinuxSandbox {
    pub fn new(cfg: SandboxConfig) -> Self {
        Self {
            cfg,
            agent_port: AtomicU16::new(0),
            agent_token: std::sync::RwLock::new(None),
        }
    }

    /// 核心 bind 成功后告诉沙箱自己的端口 —— 沙箱里的 `agent` 命令要靠它。
    pub fn set_agent_port(&self, port: u16) {
        self.agent_port.store(port, Ordering::Relaxed);
    }

    /// 沙箱里的 `agent` 命令调核心接口要带的凭据。
    pub fn set_agent_token(&self, token: Option<String>) {
        if let Ok(mut g) = self.agent_token.write() {
            *g = token;
        }
    }

    /// rootfs 齐了吗（安卓上解压是后台做的，之前只是个空壳）。
    pub fn is_ready(&self) -> bool {
        is_alpine_rootfs(&self.cfg.rootfs)
    }

    /// 常用工具装过了吗。
    ///
    /// 标记文件里存的是**清单版本**（工具数）。只判文件在不在的话，以后往
    /// `TOOLSET` 里加工具，老设备永远装不上 —— 它会一直以为自己装过了。
    pub fn is_provisioned(&self) -> bool {
        std::fs::read_to_string(self.cfg.rootfs.join("etc/agent-tools"))
            .map(|s| s.trim() == TOOLSET.len().to_string())
            .unwrap_or(false)
    }

    /// 把常用工具装上（`agent-setup`）。幂等，装过就秒退。
    ///
    /// 超时给得很大：手机上首次要下 ~75MB。
    pub async fn provision(&self) -> anyhow::Result<ExecResult> {
        self.run("/usr/local/bin/agent-setup", Duration::from_secs(900))
            .await
    }

    /// 从环境变量构造（桌面开发用）。
    ///
    /// `AGENT_LINUX_ROOTFS` 指向 rootfs；`AGENT_LINUX_PROOT` 可选，默认从 PATH 找 `proot`。
    pub fn from_env() -> Option<Self> {
        let rootfs = std::env::var("AGENT_LINUX_ROOTFS").ok()?;
        let rootfs = PathBuf::from(rootfs);

        // 注意：**不能用 `rootfs/bin/sh` 判断**。Alpine 里它是指向
        // 绝对路径 `/bin/busybox` 的符号链接，`Path::exists()` 会拿
        // 宿主机根的 `/bin/busybox` 去解析 —— 在宿主上没有，返回 false。
        // 所以查真实存在的普通文件。
        if !is_alpine_rootfs(&rootfs) {
            tracing::warn!(
                "AGENT_LINUX_ROOTFS={} 看起来不是 Alpine rootfs（找不到 bin/busybox）",
                rootfs.display()
            );
            return None;
        }

        let proot = std::env::var("AGENT_LINUX_PROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("proot"));

        let sb = Self::new(SandboxConfig {
            proot,
            rootfs,
            binds: Vec::new(),
            tmp: None,
            loader: None,
            loader32: None,
            dns: Vec::new(),
            tz_offset_minutes: None,
            env: BTreeMap::new(),
            workspace: None,
            shared: None,
        });
        if let Err(e) = sb.prepare_rootfs() {
            tracing::warn!("准备 rootfs 失败: {e:#}");
        }
        Some(sb)
    }

    pub fn config(&self) -> &SandboxConfig {
        &self.cfg
    }

    pub fn rootfs(&self) -> &Path {
        &self.cfg.rootfs
    }

    /// 启动前把 rootfs 里缺的基础文件补上。幂等。
    ///
    /// 目前做两件事：
    /// 1. `/etc/resolv.conf` —— 不写的话 `apk add` 直接挂
    /// 2. `/root`、`/tmp`、挂载点 `/workspace` 和 `/sdcard`
    ///    —— `-w /workspace` 需要它存在；挂载点事先建好比让 proot 虚拟出来稳
    pub fn prepare_rootfs(&self) -> anyhow::Result<()> {
        let rootfs = &self.cfg.rootfs;

        for d in ["root", "tmp", "var/tmp", "workspace", "sdcard"] {
            std::fs::create_dir_all(rootfs.join(d))?;
        }

        // 沙箱里的 `agent` 命令：让模型能查看 / 操作自己
        write_if_changed(&rootfs.join("usr/local/bin/agent"), AGENT_CLI, true)?;
        // 开浏览器：$BROWSER / xdg-open 都指向它（见 AGENT_OPEN）
        write_if_changed(&rootfs.join("usr/local/bin/agent-open"), AGENT_OPEN, true)?;
        for alias in [
            "xdg-open",
            "sensible-browser",
            "www-browser",
            "x-www-browser",
            "gnome-open",
            "kde-open",
        ] {
            write_if_changed(
                &rootfs.join("usr/local/bin").join(alias),
                BROWSER_SHIM,
                true,
            )?;
        }
        // 预装工具的那个脚本（App 启动后会自动跑一次，也可以手动跑）
        write_if_changed(
            &rootfs.join("usr/local/bin/agent-setup"),
            &setup_script(),
            true,
        )?;

        let resolv = rootfs.join("etc/resolv.conf");
        let servers = self.resolved_dns();
        if !servers.is_empty() {
            let body: String = servers
                .iter()
                .map(|s| format!("nameserver {s}\n"))
                .collect();
            // 只在内容变了才写，避免每次启动都碰文件
            let same = std::fs::read_to_string(&resolv)
                .map(|old| old == body)
                .unwrap_or(false);
            if !same {
                std::fs::create_dir_all(rootfs.join("etc"))?;
                std::fs::write(&resolv, body)?;
                tracing::info!("写入 resolv.conf: {servers:?}");
            }
        } else {
            tracing::warn!("没有可用的 DNS 服务器 —— 沙箱里的 apk/curl 会解析不了域名");
        }

        Ok(())
    }

    fn resolved_dns(&self) -> Vec<String> {
        if !self.cfg.dns.is_empty() {
            return self.cfg.dns.clone();
        }
        // 环境变量覆盖
        if let Ok(v) = std::env::var("AGENT_LINUX_DNS") {
            let list: Vec<String> = v
                .split([',', ' '])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !list.is_empty() {
                return list;
            }
        }
        // 桌面兜底：抄宿主机的
        if let Ok(text) = std::fs::read_to_string("/etc/resolv.conf") {
            let list: Vec<String> = text
                .lines()
                .filter_map(|l| l.trim().strip_prefix("nameserver "))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && !s.starts_with("127."))
                .take(3)
                .collect();
            if !list.is_empty() {
                return list;
            }
        }
        vec!["1.1.1.1".into(), "8.8.8.8".into()]
    }

    /// 沙箱里有没有真 bash（没装就只能退回 ash，见 `command_line`）。
    pub fn has_bash(&self) -> bool {
        self.cfg.rootfs.join("bin/bash").is_file()
    }

    /// 构造完整命令行。抽出来单独测试，不需要真的跑 proot。
    ///
    /// 对应 OpenMinis `PRootKernel.buildProotCommand()`：
    /// ```text
    /// <proot> -0 -l -r <rootfs> -b /dev -b /proc -b /sys -w /workspace
    ///         [-b <host>:<linux> ...] [/bin/bash | /bin/sh] -c "<script>"
    /// ```
    pub fn command_line(&self, script: &str) -> Vec<String> {
        let mut cmd: Vec<String> = Vec::with_capacity(20);

        macro_rules! arg {
            ($($x:expr),+ $(,)?) => {{ $( cmd.push($x.to_string()); )+ }};
        }

        arg!(self.cfg.proot.display().to_string());

        // 假装 uid 0。Alpine 的 apk 装包会因为不是 root 而失败。
        arg!("-0");

        // --link2symlink：Android 的 /data 拒绝 app uid 跨目录 link()，
        // 而 apk 装 busybox-applet 包（binutils/gcc 依赖）全靠硬链接
        // ar/ld/nm/strip -> busybox。不加则每次装包都 EPERM。
        // 桌面上无害，所以两边用同一套参数，避免行为分叉。
        arg!("--link2symlink");

        arg!("-r", self.cfg.rootfs.display().to_string());

        for fs in ["/dev", "/proc", "/sys"] {
            arg!("-b", fs);
        }

        for (host, guest) in &self.cfg.binds {
            arg!("-b", format!("{host}:{guest}"));
        }
        if let Some(h) = &self.cfg.shared {
            arg!("-b", format!("{}:{SHARED}", h.display()));
        }
        if let Some(h) = &self.cfg.workspace {
            arg!("-b", format!("{}:{WORKSPACE}", h.display()));
        }

        // 子进程随 proot 一起死，避免留下孤儿
        arg!("--kill-on-exit");

        arg!("-w", WORKSPACE);

        // 装过 bash 就用 bash：模型写的命令常带 bash 语法（`[[ ]]`、数组、
        // 进程替换、`<<<`），拿 ash 跑会报 "syntax error"。首次预装之前
        // （或者手动清了 rootfs）没有 bash，退回 /bin/sh（这时结果里会
        // 提醒模型去装，见 `ExecResult::to_text`）。
        let shell = if self.has_bash() {
            "/bin/bash"
        } else {
            "/bin/sh"
        };
        arg!(shell, "-c", script);

        cmd
    }

    /// 沙箱内的环境变量。
    ///
    /// 注意这里**不用 `putIfAbsent` 式的"继承宿主环境"** —— 宿主环境
    /// 带进来反而有害（PATH 就是典型）。显式给一份干净的白名单。
    fn guest_env(&self) -> Vec<(String, String)> {
        let mut env: BTreeMap<String, String> = BTreeMap::new();

        env.insert("PATH".into(), GUEST_PATH.into());
        env.insert("HOME".into(), "/root".into());
        // ash 非登录 shell 也会读 ENV 指向的文件，这样 profile 里的
        // 别名/HISTFILE 才生效（我们用 /bin/sh -c，属于非登录 shell）
        env.insert("ENV".into(), "/etc/profile".into());
        env.insert("CHARSET".into(), "UTF-8".into());
        env.insert("LANG".into(), "C.UTF-8".into());
        // 非交互，别输出 ANSI
        env.insert("TERM".into(), "dumb".into());
        // 沙箱里没有浏览器，但手机上有：把 BROWSER 指到 agent-open，它让手机
        // 去打开（`agent open` → 设备通道）。`gh auth login --web` / python
        // webbrowser / xdg-open 都会走这里。强制覆盖（不 putIfAbsent）：用户
        // dotfile 里的 BROWSER= 不该抢走它。
        env.insert("BROWSER".into(), "/usr/local/bin/agent-open".into());
        // 分页器会**挂住**（非交互环境没人按键），一次工具调用就废了
        env.insert("PAGER".into(), "cat".into());
        env.insert("GIT_PAGER".into(), "cat".into());
        env.insert("NO_COLOR".into(), "1".into());
        env.insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
        env.insert("GOMAXPROCS".into(), "2".into()); // 手机上限
                                                     // proot 的 link2symlink 会留 .l2s.* 哨兵文件，uv 默认 hardlink
                                                     // 模式会二次 link 它们并拿到 EPERM
        env.insert("UV_LINK_MODE".into(), "symlink".into());
        // TZ 必须是 POSIX 格式（符号与 UTC 偏移相反）：musl 的 TZ 解析器
        // 认不出 "GMT+8" 这种带 +/- 的缩写
        env.insert("TZ".into(), posix_tz(self.cfg.tz_offset_minutes));

        // 沙箱里的 `agent` 命令靠它找到核心（App 在 bind 成功后注入）
        let port = self.agent_port.load(Ordering::Relaxed);
        if port != 0 {
            env.insert("AGENT_PORT".into(), port.to_string());
            env.insert("AGENT_BASE".into(), format!("http://127.0.0.1:{port}"));
        }
        if let Some(t) = self.agent_token.read().ok().and_then(|g| g.clone()) {
            env.insert("AGENT_TOKEN".into(), t);
        }

        // 给 guest 程序用的临时目录
        if let Some(tmp) = &self.cfg.tmp {
            env.insert("TMPDIR".into(), tmp.display().to_string());
            // proot 自己用的（宿主路径）—— 名字和 TMPDIR 不一样，别搞混
            env.insert("PROOT_TMP_DIR".into(), tmp.display().to_string());
        }
        // 安卓必需：指向 nativeLibraryDir 里的 loader。
        // 不设的话 proot 会走内嵌 loader 解压路径，撞上 W^X。
        if let Some(l) = &self.cfg.loader {
            env.insert("PROOT_LOADER".into(), l.display().to_string());
        }
        if let Some(l) = &self.cfg.loader32 {
            env.insert("PROOT_LOADER_32".into(), l.display().to_string());
        }
        for (k, v) in &self.cfg.env {
            env.insert(k.clone(), v.clone());
        }

        env.into_iter().collect()
    }

    /// 在沙箱里跑一段 shell 脚本。
    pub async fn run(&self, script: &str, timeout: Duration) -> anyhow::Result<ExecResult> {
        self.run_live(script, timeout, None).await
    }

    /// 同 `run`，但把输出**边跑边**交给 `live`（stdout/stderr 按到达顺序混在一起）。
    /// 给前端实时显示用；最终结果照样完整返回。
    pub async fn run_live(
        &self,
        script: &str,
        timeout: Duration,
        live: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    ) -> anyhow::Result<ExecResult> {
        let argv = self.command_line(script);
        let using_bash = self.has_bash();
        let (bin, args) = argv.split_first().expect("command_line 至少有一个元素");

        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(args);
        cmd.env_clear();
        for (k, v) in self.guest_env() {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // 超时 / 按停止（future 被丢掉）时杀掉 proot；沙箱里的子孙进程靠
        // `--kill-on-exit` 跟着一起死（见 `command_line`）
        cmd.kill_on_drop(true);

        if let Some(tmp) = &self.cfg.tmp {
            let _ = std::fs::create_dir_all(tmp);
        }

        let started = Instant::now();
        let mut child = cmd.spawn()?;

        // 边跑边读，内存里只留**首尾两端**（见 `Capture`）。
        //
        // 原来是 wait_with_output() —— 整段输出全攒在内存里。`yes`、
        // `cat /dev/urandom | base64` 这类命令 60 秒能吐出几个 GB，
        // 手机上直接 OOM 带走整个 App。
        // 边跑边读；结果塞进共享的 `Capture`（为什么要共享见 `drain`）。
        let out_cap = Arc::new(Mutex::new(Capture::default()));
        let err_cap = Arc::new(Mutex::new(Capture::default()));
        let out_task = tokio::spawn(Capture::read_into(
            child.stdout.take(),
            live.clone(),
            out_cap.clone(),
        ));
        let err_task = tokio::spawn(Capture::read_into(
            child.stderr.take(),
            live,
            err_cap.clone(),
        ));

        let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(r) => (Some(r?), false),
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                (None, true)
            }
        };

        // 管道可能被沙箱里的孙进程继承着不关（`nohup … &`）—— 别为它无限
        // 等，但也**别把已经读到的丢了**（见 `drain`）。
        let grace = Duration::from_secs(2);
        let stdout = drain(out_task, &out_cap, grace).await;
        let stderr = drain(err_task, &err_cap, grace).await;
        let (stdout_text, t1) = stdout.render();
        let (stderr_text, t2) = stderr.render();
        // 中间被扔掉（输出超过内存上限）也算截断
        let dropped = stdout.dropped_bytes + stderr.dropped_bytes;
        let truncated = t1 || t2 || dropped > 0;
        let status = status.unwrap_or_default();

        // 截断了就把捕获到的输出落进沙箱的 /tmp，并告诉模型路径 ——
        // 它可以再用 grep/sed 去翻，不用把几十 MB 塞回上下文
        let full_output_path = if truncated {
            self.spill_captured(&stdout, &stderr)
        } else {
            None
        };

        // 信号：超时那次是我们自己 SIGKILL 的，报了反而混淆
        use std::os::unix::process::ExitStatusExt;
        let signal = if timed_out { None } else { status.signal() };

        Ok(ExecResult {
            code: if timed_out {
                -1
            } else {
                status.code().unwrap_or(-1)
            },
            signal,
            stdout: stdout_text,
            stderr: stderr_text,
            duration_ms: started.elapsed().as_millis() as u64,
            truncated,
            // 超时也把已经产出的输出带回去 —— 原来直接丢空，模型只知道
            // "超时了"，看不到卡在哪一步
            timed_out,
            full_output_path,
            using_bash,
        })
    }

    /// 把捕获到的输出写进 rootfs 的 `/tmp`，返回沙箱内的路径。
    ///
    /// 中间被扔掉过的话（输出超过内存上限）在文件里也标出来，免得模型
    /// grep 的时候以为前后是连着的。
    fn spill_captured(&self, stdout: &Capture, stderr: &Capture) -> Option<String> {
        let name = format!("agent-out-{}.log", uuid::Uuid::now_v7());
        let host = self.cfg.rootfs.join("tmp").join(&name);
        std::fs::create_dir_all(host.parent()?).ok()?;

        let mut body = Vec::new();
        stdout.write_to(&mut body);
        if !stderr.is_empty() {
            body.extend_from_slice(b"\n--- stderr ---\n");
            stderr.write_to(&mut body);
        }
        std::fs::write(&host, body).ok()?;
        Some(format!("/tmp/{name}"))
    }

    // -----------------------------------------------------------------------
    // 文件访问
    // -----------------------------------------------------------------------

    /// 是否挂了手机存储
    pub fn has_shared_storage(&self) -> bool {
        self.cfg.shared.is_some()
    }

    /// 沙箱内路径 → 宿主机路径。
    ///
    /// - `/root/a.txt` → `<rootfs>/root/a.txt`
    /// - `a.txt`（相对） → 按 cwd `/workspace` 算
    /// - `/workspace/…`、`/sdcard/…` → 挂载的宿主目录（有挂的话）
    ///
    /// 两道越界检查：先做**词法归一化**（处理 `.` 和 `..`），
    /// 文件存在时再 `canonicalize` 一次，堵住符号链接逃逸 ——
    /// Alpine 里大量条目是指向绝对路径的符号链接。
    pub fn resolve_path(&self, guest: &str) -> anyhow::Result<PathBuf> {
        self.resolve_path_kind(guest, false)
    }

    /// 目标还**不存在**（`edit` 的 create 模式）时的路径解析。
    ///
    /// `confine` 的真实路径检查靠 `canonicalize`，新文件查不了 —— 得往上找
    /// 最近一个存在的祖先（见 `confine_new`）。
    pub fn resolve_new_path(&self, guest: &str) -> anyhow::Result<PathBuf> {
        self.resolve_path_kind(guest, true)
    }

    fn resolve_path_kind(&self, guest: &str, for_new: bool) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(!guest.trim().is_empty(), "路径为空");

        // 先在沙箱的视角里归一化，再决定落到哪个宿主目录
        let abs = if guest.starts_with('/') {
            PathBuf::from(guest)
        } else {
            Path::new(WORKSPACE).join(guest)
        };
        let g = lexical_normalize(&abs);

        let check = |root: &Path, target: PathBuf| -> anyhow::Result<PathBuf> {
            if for_new {
                confine_new(root, &target, guest)
            } else {
                confine(root, &target, guest)
            }
        };

        let mounts = [(SHARED, &self.cfg.shared), (WORKSPACE, &self.cfg.workspace)];
        for (at, host) in mounts {
            let Some(host) = host else { continue };
            if let Ok(rest) = g.strip_prefix(at) {
                let root = lexical_normalize(host);
                return check(&root, root.join(rest));
            }
        }

        let root = lexical_normalize(&self.cfg.rootfs);
        let rel = g.strip_prefix("/").unwrap_or(&g);
        check(&root, root.join(rel))
    }

    /// `edit` 工具：读文件 → 按原文替换 → 写回。
    ///
    /// 整个过程读一次写一次，中间不落盘（见 `editor` 模块的设计说明）。
    pub fn edit_file(&self, guest_path: &str, edits: &[Edit]) -> anyhow::Result<EditOutcome> {
        let host = self.resolve_path(guest_path)?;
        anyhow::ensure!(
            host.is_file(),
            "不是文件或不存在: {guest_path}（要新建文件就给 content，不要用 edits）"
        );

        let raw = std::fs::read_to_string(&host)
            .map_err(|e| anyhow::anyhow!("读取失败 {guest_path}: {e}（只能编辑 UTF-8 文本）"))?;

        let outcome = editor::apply(&raw, edits, guest_path)?;
        atomic_write(&host, outcome.content.as_bytes())
            .map_err(|e| anyhow::anyhow!("写入失败 {guest_path}: {e}"))?;

        Ok(outcome)
    }

    /// `edit` 工具的 create 模式：新建一个文件（父目录按需建）。
    ///
    /// 已存在就报错，**不静默覆盖** —— 想改已有文件应该先 `read` 再用 `edits`，
    /// 那样改错了还能在 diff 里看出来；直接盖掉等于把原文扔了。
    pub fn create_file(&self, guest_path: &str, content: &str) -> anyhow::Result<CreatedFile> {
        let host = self.resolve_new_path(guest_path)?;
        anyhow::ensure!(
            !host.exists(),
            "已存在: {guest_path}（content 只用于新建；要改它请先 read，再用 edits）"
        );

        if let Some(dir) = host.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| anyhow::anyhow!("建目录失败 {}: {e}", dir.display()))?;
        }
        atomic_write(&host, content.as_bytes())
            .map_err(|e| anyhow::anyhow!("写入失败 {guest_path}: {e}"))?;

        Ok(CreatedFile {
            lines: content.lines().count(),
            bytes: content.len(),
        })
    }

    /// `read` 工具：先嗅魔数，是图片就走图片那条路（缩放 + 内联），
    /// 否则当 UTF-8 文本读（offset/limit/头截断，见 `reader` 模块）。
    pub fn read_any(
        &self,
        guest_path: &str,
        offset: usize,
        limit: Option<usize>,
        numbered: bool,
    ) -> anyhow::Result<ReadResult> {
        let host = self.resolve_readable(guest_path)?;
        // 先嗅头（只读 12 字节）再决定走哪条路：不能为了判断“是不是图片”
        // 就把一个可能上百 MB 的文本文件整份读进来。
        if crate::imaging::is_image_file(&host)? {
            if let Some(img) = crate::imaging::load_if_image(&host)? {
                return Ok(ReadResult::Image(img));
            }
        }
        let text = read_utf8(&host, guest_path)?;
        Ok(ReadResult::Text(reader::read(
            &text, guest_path, offset, limit, numbered,
        )?))
    }

    /// 纯文本那条路（测试与内部用）。
    pub fn read_text(
        &self,
        guest_path: &str,
        offset: usize,
        limit: Option<usize>,
        numbered: bool,
    ) -> anyhow::Result<ReadView> {
        let host = self.resolve_readable(guest_path)?;
        let text = read_utf8(&host, guest_path)?;
        reader::read(&text, guest_path, offset, limit, numbered)
    }

    fn resolve_readable(&self, guest_path: &str) -> anyhow::Result<PathBuf> {
        let host = self.resolve_path(guest_path)?;
        anyhow::ensure!(
            host.is_file(),
            "不是文件或不存在: {guest_path}（目录内容用 bash 的 ls）"
        );
        Ok(host)
    }

    /// 写文件（`bash` 之外的小工具，也供测试用）
    pub fn read_file(&self, guest_path: &str) -> anyhow::Result<String> {
        let host = self.resolve_path(guest_path)?;
        std::fs::read_to_string(&host).map_err(|e| anyhow::anyhow!("读取失败 {guest_path}: {e}"))
    }
}

/// `path` 必须落在 `root` 里面：词法上一次，存在的话再按真实路径查一次
fn confine(root: &Path, path: &Path, guest: &str) -> anyhow::Result<PathBuf> {
    let norm = lexical_normalize(path);
    anyhow::ensure!(norm.starts_with(root), "路径越界，不允许走出沙箱: {guest}");
    if let Ok(real) = norm.canonicalize() {
        let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        anyhow::ensure!(real.starts_with(&real_root), "符号链接指向沙箱外: {guest}");
    }
    Ok(norm)
}

/// 给**还不存在**的目标做 confinement。
///
/// 目标自己 `canonicalize` 不了，就往上找最近一个存在的祖先，看它的真实路径
/// 在不在沙箱里。这步不能省：模型能在沙箱里 `ln -s /etc /workspace/link`，而
/// 我们是在**宿主**上按路径写文件的 —— 不查就写到沙箱外面去了。
///
/// （`read` / `edit` 那条路不用它：文件已存在，`confine` 的 `canonicalize`
/// 已经兜住了。所以它是给 create 专用。）
fn confine_new(root: &Path, path: &Path, guest: &str) -> anyhow::Result<PathBuf> {
    let norm = lexical_normalize(path);
    anyhow::ensure!(norm.starts_with(root), "路径越界，不允许走出沙箱: {guest}");
    let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    let mut probe = norm.clone();
    while probe.starts_with(root) {
        match probe.canonicalize() {
            Ok(real) => {
                anyhow::ensure!(real.starts_with(&real_root), "符号链接指向沙箱外: {guest}");
                return Ok(norm);
            }
            // 连 root 都不存在（沙箱还没起）就无从检查，交给后面的 IO 报错
            Err(_) if probe == root => break,
            Err(_) => {
                if !probe.pop() {
                    break;
                }
            }
        }
    }
    Ok(norm)
}

/// 整份写入一个文件，**要么全是新的、要么还是旧的**。
///
/// `std::fs::write` 是先清空再写：写到一半进程被杀（系统回收、用户划掉 App），
/// 文件就只剩半截甚至是空的 —— 而 `/workspace` 是用户内部存储里的文件夹，坏了
/// 就是真坏了。所以先写同目录下的临时文件、刷盘，再 rename 盖过去（同一个文件
/// 系统里 rename 是原子的）。
///
/// - 目标是**符号链接**：写到它指向的那个文件，不要把链接本身换成普通文件
///   （原来 `fs::write` 也是跟着链接走的）。越界检查调用方已经做过（`confine`）。
/// - 改已有文件时保留原来的权限位（不然可执行脚本改一下就不能执行了）。
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let target = match std::fs::canonicalize(path) {
        Ok(real) => real,
        Err(_) => path.to_path_buf(), // 新文件
    };
    let dir = target.parent().unwrap_or(Path::new("."));
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let tmp = dir.join(format!(
        ".{name}.agent-{}.tmp",
        uuid::Uuid::now_v7().simple()
    ));
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        if let Ok(meta) = std::fs::metadata(&target) {
            // 共享存储（FUSE）上改权限可能不被支持：失败不算错
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, &target)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// 读成 UTF-8 文本。二进制给一条能操作的错误，而不是一句 "invalid utf-8"。
fn read_utf8(host: &Path, guest_path: &str) -> anyhow::Result<String> {
    let size = std::fs::metadata(host).map(|m| m.len()).unwrap_or(0);
    check_text_size(size, guest_path)?;
    let raw = std::fs::read(host).map_err(|e| anyhow::anyhow!("读取失败 {guest_path}: {e}"))?;
    String::from_utf8(raw).map_err(|_| {
        anyhow::anyhow!(
            "不是 UTF-8 文本（二进制文件？）: {guest_path}；要查看内容用 bash 的 file / xxd"
        )
    })
}

/// 文本大小闸门。单独抽出来是为了能测（不用真写一个 32MB 的文件）。
fn check_text_size(size: u64, guest_path: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        size <= MAX_TEXT_BYTES,
        "文件 {}MB，超过 {}MB：不能整份读进内存（会 OOM）。\
         用 bash 局部读，例如 `sed -n '1,2000p' {guest_path}` 或 `head -c 50000 {guest_path}`",
        size / 1024 / 1024,
        MAX_TEXT_BYTES / 1024 / 1024
    );
    Ok(())
}

/// 词法归一化：处理 `.` / `..` / 重复分隔符，**不碰文件系统**。
///
/// 不能用 `canonicalize` 代替：目标文件可能还不存在，而且 rootfs 里
/// 大量绝对符号链接会被解到宿主机上去。
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            // 在根目录上 `..` 是空操作（PathBuf::pop 对 "/" 返回 false）
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// rootfs 看起来是不是真的 Alpine。
///
/// 只看**普通文件**，因为 Alpine 里大量条目是指向绝对路径的符号链接，
/// 在宿主机上解析会失败。
fn is_alpine_rootfs(rootfs: &Path) -> bool {
    rootfs.join("bin/busybox").is_file() || rootfs.join("etc/alpine-release").is_file()
}

/// 一路输出里保留的两端：**头部**和**尾部**。
///
/// 回给模型的是「首 100 行 + `[省略 N 行]` + 尾 100 行」（见 `render`），
/// 所以两端都得留着。超过内存上限时扔的是**中间**：头部攒满 `CAPTURE_HEAD`
/// 就封口（切在行边界上），后面的进尾部；尾部超过 `CAPTURE_TAIL` 就从最旧的
/// 一端挤掉，挤掉多少记在 `dropped_*` 里。
///
/// 内存上界 = `CAPTURE_HEAD + CAPTURE_TAIL`，跟原来一个 4 MB 的缓冲一样。
#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    tail: Vec<u8>,
    /// 中间扔掉的字节数
    dropped_bytes: u64,
    /// 中间扔掉的整行数；单行超长、只能从行中间扔时算不进来
    dropped_lines: u64,
}

const CAPTURE_HEAD: usize = 2 << 20;
const CAPTURE_TAIL: usize = 2 << 20;

impl Capture {
    /// 边读边往里塞；`live` 是给前端的实时副本。
    ///
    /// 结果放在共享的 `Arc<Mutex<Capture>>` 里、而不是当返回值：管道可能被
    /// 孙进程继承着不关（`nohup … &`），读任务会一直卡在 `read()` 上 —— 父
    /// 进程等不到它，但**已经读到的输出必须拿得走**（见 `drain`）。
    async fn read_into<R: tokio::io::AsyncRead + Unpin>(
        src: Option<R>,
        live: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
        cap: Arc<Mutex<Capture>>,
    ) {
        use tokio::io::AsyncReadExt;
        let Some(mut src) = src else { return };
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match src.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(tx) = &live {
                        let _ = tx.send(chunk[..n].to_vec());
                    }
                    lock(&cap).push(&chunk[..n]);
                }
            }
        }
    }

    fn push(&mut self, data: &[u8]) {
        let mut rest = data;
        if self.head.len() < CAPTURE_HEAD {
            let take = (CAPTURE_HEAD - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            // 头刚封口：把最后那半行挪进尾部，让头停在行边界上
            if self.head.len() >= CAPTURE_HEAD {
                if let Some(nl) = self.head.iter().rposition(|&b| b == b'\n') {
                    let leftover = self.head.split_off(nl + 1);
                    self.tail.extend_from_slice(&leftover);
                }
            }
        }
        if !rest.is_empty() {
            self.push_tail(rest);
        }
    }

    /// 往尾部追加；超过 `CAPTURE_TAIL` 就从最旧的一端挤掉。
    fn push_tail(&mut self, data: &[u8]) {
        self.tail.extend_from_slice(data);
        if self.tail.len() <= CAPTURE_TAIL {
            return;
        }
        // 挤到行边界（整行扔）；单行超长找不到换行，就按字节扔 ——
        // 但别整段都丢，那样尾部就永远攒不起来了
        let excess = self.tail.len() - CAPTURE_TAIL;
        let cut = match self.tail[excess..].iter().position(|&b| b == b'\n') {
            Some(i) => excess + i + 1,
            None => excess,
        };
        self.dropped_lines += self.tail[..cut].iter().filter(|&&b| b == b'\n').count() as u64;
        self.dropped_bytes += cut as u64;
        self.tail.drain(..cut);
    }

    fn is_empty(&self) -> bool {
        self.head.is_empty() && self.tail.is_empty()
    }

    /// 渲染成给模型看的样子：首 `HEAD_LINES` 行 + `[省略 N 行]` + 尾 `TAIL_LINES` 行。
    ///
    /// 没超上限就原样返回。第二个返回值是「有没有截断」。
    fn render(&self) -> (String, bool) {
        if self.is_empty() {
            return (String::new(), false);
        }
        let head = String::from_utf8_lossy(&self.head);
        let head_lines = real_lines(&head);
        // 尾部为空 = 没扔过中间：整份输出都在 `head` 里，首尾取自同一份
        let split = !self.tail.is_empty();
        if !split && head_lines.len() <= MAX_OUTPUT_LINES && head.len() <= MAX_OUTPUT_BYTES {
            return (head.into_owned(), false);
        }

        let tail_text = String::from_utf8_lossy(&self.tail);
        let tail_lines = if split {
            real_lines(&tail_text)
        } else {
            Vec::new()
        };

        let (head_out, head_shown) = head_part(&head_lines, HEAD_LINES, SIDE_BYTES);
        let (tail_out, tail_shown) = if split {
            tail_part(&tail_lines, TAIL_LINES, SIDE_BYTES)
        } else {
            // 同一份行：尾部只能取头没拿走的那些
            tail_part(&head_lines[head_shown..], TAIL_LINES, SIDE_BYTES)
        };

        // 省略了多少行：头尾没展示的 + 中间扔掉的
        // （没分家时首尾取自同一份行，尾巴拿走的那些不能重复计入“省略”）
        let omitted_lines = if split {
            (head_lines.len() - head_shown)
                + self.dropped_lines as usize
                + (tail_lines.len() - tail_shown)
        } else {
            head_lines.len() - head_shown - tail_shown
        };
        // 省略了多少字节：留下的里没展示的 + 中间扔掉的
        let kept = (self.head.len() + self.tail.len()) as u64;
        let shown = (head_out.len() + tail_out.len()) as u64;
        let omitted_bytes = kept.saturating_sub(shown) + self.dropped_bytes;

        // 正常报「省略 N 行」；单行超长、整行数不出来时退回报字节
        let marker = if omitted_lines > 0 {
            format!("[省略 {omitted_lines} 行]")
        } else {
            format!("[省略 {}]", crate::reader::fmt_size(omitted_bytes as usize))
        };

        let mut out = String::with_capacity(head_out.len() + tail_out.len() + marker.len() + 4);
        if !head_out.is_empty() {
            out.push_str(&head_out);
            out.push('\n');
        }
        out.push_str(&marker);
        out.push('\n');
        out.push_str(&tail_out);
        // 原文结尾有换行就保留
        let ended_nl = if split {
            tail_text.ends_with('\n')
        } else {
            head.ends_with('\n')
        };
        if ended_nl && !tail_out.is_empty() {
            out.push('\n');
        }
        (out, true)
    }

    /// 落盘用：头 +（中间扔掉的提示）+ 尾。
    ///
    /// 中间扔过东西就必须标出来，不然模型 grep 这份文件会以为前后是连着的。
    fn write_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.head);
        if self.tail.is_empty() {
            return;
        }
        if self.dropped_bytes > 0 {
            let note = if self.dropped_lines > 0 {
                format!(
                    "\n[... 中间省略 {} 行 / {} ...]\n",
                    self.dropped_lines,
                    crate::reader::fmt_size(self.dropped_bytes as usize)
                )
            } else {
                format!(
                    "\n[... 中间省略 {} ...]\n",
                    crate::reader::fmt_size(self.dropped_bytes as usize)
                )
            };
            out.extend_from_slice(note.as_bytes());
        }
        out.extend_from_slice(&self.tail);
    }
}

/// 拿 `Capture` 的锁；被上个任务的 panic 毒到了也照样用（里面只有字节，
/// 没有需要维持的不变式）。
fn lock(cap: &Mutex<Capture>) -> MutexGuard<'_, Capture> {
    cap.lock().unwrap_or_else(|e| e.into_inner())
}

/// 等读任务收尾，把捕获到的输出取走。
///
/// 正常情况下管道会 EOF、任务自己结束。被孙进程占着的话最多等 `grace`，
/// 超时就 abort 掉任务 —— 要的是**已读到的输出**，不是干净的收尾。
///
/// 原来这里是「等 2 秒，超时就用 `unwrap_or_default()`」：那个默认值是**空的
/// `Capture`**，等于把 2 秒内已经读到的输出全扔了。一条 `nohup x &` 就能让
/// 模型看到空输出，而且看不出来为什么。
async fn drain(
    mut task: tokio::task::JoinHandle<()>,
    cap: &Mutex<Capture>,
    grace: Duration,
) -> Capture {
    match tokio::time::timeout(grace, &mut task).await {
        // 任务自己收尾了 —— 注意：这时 `task` **已经被 poll 完**，
        // 再来一个 `task.await` 会 panic（“polled after completion”），
        // 而 release 是 `panic = "abort"` —— 每次 bash 调用都会把 App 干掉。
        Ok(_) => {}
        Err(_) => {
            // 管道还被孙进程占着：不等了，但要把已读到的拿走。
            // abort 之后才允许再 await（这时它确实还没完成）。
            task.abort();
            let _ = task.await;
        }
    }
    std::mem::take(&mut *lock(cap))
}

/// 按 `reader` 的规则拆行：结尾 `\n` 切出来的空串不算一行。
fn real_lines(text: &str) -> Vec<&str> {
    match text.strip_suffix('\n') {
        Some(t) => t.split('\n').collect(),
        None => text.split('\n').collect(),
    }
}

/// 取头部 ≤`want` 行、≤`budget` 字节。返回 (正文, 算作「已展示」的行数)。
///
/// 第一行就超预算时给它的前半截 —— 宁可给半行，也别让模型什么都看不到
/// （省掉的那部分会进 `[省略 …]`）。
fn head_part(lines: &[&str], want: usize, budget: usize) -> (String, usize) {
    let mut out = String::new();
    let mut shown = 0usize;
    for line in lines.iter().take(want) {
        let sep = usize::from(shown > 0);
        if out.len() + sep + line.len() > budget {
            if shown == 0 {
                out.push_str(clip_head(line, budget));
                return (out, 1);
            }
            break;
        }
        if shown > 0 {
            out.push('\n');
        }
        out.push_str(line);
        shown += 1;
    }
    (out, shown)
}

/// 取尾部 ≤`want` 行、≤`budget` 字节（从后往前凑，返回时顺序正常）。
fn tail_part(lines: &[&str], want: usize, budget: usize) -> (String, usize) {
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for line in lines.iter().rev().take(want) {
        let sep = usize::from(!kept.is_empty());
        if used + sep + line.len() > budget {
            if kept.is_empty() {
                return (clip_tail(line, budget).to_string(), 1);
            }
            break;
        }
        used += sep + line.len();
        kept.push(line);
    }
    kept.reverse();
    (kept.join("\n"), kept.len())
}

/// `s` 的前 `budget` 字节（不切开多字节字符）
fn clip_head(s: &str, budget: usize) -> &str {
    let mut end = budget.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `s` 的后 `budget` 字节（不切开多字节字符）
fn clip_tail(s: &str, budget: usize) -> &str {
    let mut start = s.len().saturating_sub(budget);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// 设备当前时区的 POSIX TZ 串。
///
/// 与 UTC 偏移符号相反：UTC+8 → `LCL-8`。
fn posix_tz(override_minutes: Option<i32>) -> String {
    let offset_min = override_minutes
        .or_else(|| {
            std::env::var("AGENT_TZ_OFFSET_MIN")
                .ok()
                .and_then(|s| s.parse::<i32>().ok())
        })
        .or_else(host_offset_minutes)
        .unwrap_or(0);

    let sign = if offset_min >= 0 { '-' } else { '+' };
    let abs = offset_min.abs();
    let (h, m) = (abs / 60, abs % 60);
    if m == 0 {
        format!("LCL{sign}{h}")
    } else {
        format!("LCL{sign}{h}:{m:02}")
    }
}

/// 桌面兜底：从 `TZ` 环境变量读偏移。没有 chrono 依赖，手写解析。
fn host_offset_minutes() -> Option<i32> {
    // TZ 环境变量优先
    if let Ok(tz) = std::env::var("TZ") {
        if let Some(v) = parse_posix_tz(&tz) {
            return Some(v);
        }
    }
    None
}

/// 解析形如 `CST-8` / `UTC+5:30` 的 POSIX TZ 串，返回 UTC 偏移分钟数
fn parse_posix_tz(s: &str) -> Option<i32> {
    let bytes = s.as_bytes();
    let i = bytes.iter().position(|&b| b == b'-' || b == b'+')?;
    let sign = if bytes[i] == b'-' { 1 } else { -1 };
    let rest = &s[i + 1..];
    let (h, m) = match rest.split_once(':') {
        Some((h, m)) => (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?),
        None => (rest.parse::<i32>().ok()?, 0),
    };
    Some(sign * (h * 60 + m))
}

#[cfg(test)]
mod tests {

    /// 测试用：正常读完一份内存里的字节（不关心管道被占着的情形）。
    async fn read_all(data: &[u8]) -> Capture {
        let cap = Arc::new(Mutex::new(Capture::default()));
        Capture::read_into(Some(data), None, cap.clone()).await;
        let out = std::mem::take(&mut *lock(&cap));
        out
    }

    #[tokio::test]
    async fn capture_keeps_both_ends_and_counts_the_middle() {
        // 模拟一个狂吐输出的命令：远超上限
        let total = CAPTURE_HEAD + CAPTURE_TAIL + 10_000;
        let mut data = b"START\n".to_vec();
        data.resize(total - 4, b'\n');
        data.extend_from_slice(b"END\n");
        let cap = read_all(&data).await;
        assert!(cap.head.starts_with(b"START\n"), "头部不能丢");
        assert!(cap.tail.ends_with(b"END\n"), "尾部不能丢");
        assert!(cap.head.len() <= CAPTURE_HEAD, "内存要有上界");
        assert!(cap.tail.len() <= CAPTURE_TAIL, "内存要有上界");
        assert!(cap.dropped_bytes > 0, "中间确实扔了");
        // 丢掉的部分就是中间那段，不多不少
        assert_eq!(
            cap.head.len() as u64 + cap.tail.len() as u64 + cap.dropped_bytes,
            total as u64
        );
    }
    use super::*;

    fn sandbox() -> LinuxSandbox {
        LinuxSandbox::new(SandboxConfig {
            proot: PathBuf::from("/native/libproot.so"),
            rootfs: PathBuf::from("/data/files/alpine-rootfs"),
            binds: vec![("/host/share".into(), "/mnt/share".into())],
            tmp: Some(PathBuf::from("/data/cache/tmp")),
            dns: vec!["10.0.0.1".into()],
            tz_offset_minutes: None,
            loader: None,
            loader32: None,
            env: BTreeMap::new(),
            workspace: None,
            shared: None,
        })
    }

    #[test]
    fn command_line_matches_the_documented_shape() {
        let argv = sandbox().command_line("echo hi");
        assert_eq!(argv[0], "/native/libproot.so");
        assert_eq!(argv[1], "-0");
        assert_eq!(argv[2], "--link2symlink");
        assert_eq!(&argv[3..5], &["-r", "/data/files/alpine-rootfs"]);
        assert_eq!(&argv[5..11], &["-b", "/dev", "-b", "/proc", "-b", "/sys"]);
        assert_eq!(&argv[11..13], &["-b", "/host/share:/mnt/share"]);
        assert_eq!(argv[13], "--kill-on-exit");
        assert_eq!(&argv[14..16], &["-w", "/workspace"]);
        assert_eq!(&argv[16..], &["/bin/sh", "-c", "echo hi"]);
    }

    #[test]
    fn script_is_passed_as_one_argv_element_not_shell_escaped() {
        // 关键：脚本原样作为单个参数传，不做任何转义 —— 因为我们是
        // 直接 execve，没有中间 shell 会二次解析它
        let nasty = "echo \"a b\"; ls | grep x; echo '$(whoami)'";
        let argv = sandbox().command_line(nasty);
        assert_eq!(argv.last().unwrap(), nasty);
    }

    /// 没 bash 退回 ash、装了就用 bash —— 两条路都要盯住。
    #[test]
    fn command_line_uses_bash_once_it_is_installed() {
        let t = TempRoot::new("shellbash");
        let sb = t.sandbox();
        let shell_of = |argv: Vec<String>| {
            let i = argv.iter().position(|a| a == "-c").expect("-c 一定在");
            argv[i - 1].clone()
        };
        assert_eq!(
            shell_of(sb.command_line("true")),
            "/bin/sh",
            "没装就退回 ash"
        );
        std::fs::write(t.dir.join("bin/bash"), b"").unwrap();
        assert_eq!(
            shell_of(sb.command_line("true")),
            "/bin/bash",
            "装了就该用 bash，不然模型写的 bash 语法白写"
        );
    }

    /// 管道正常关闭（常见情形）：drain 不能多 await 一次 —— 那条路上
    /// `JoinHandle` 已经被 poll 完，再 poll 会 panic 掉整个 App。
    #[tokio::test]
    async fn drain_handles_a_task_that_finishes_in_time() {
        let cap = Arc::new(Mutex::new(Capture::default()));
        let task = tokio::spawn(Capture::read_into(Some(&b"hello\n"[..]), None, cap.clone()));
        let got = drain(task, &cap, Duration::from_secs(5)).await;
        assert_eq!(got.head, b"hello\n");
        assert_eq!(got.dropped_bytes, 0);
    }

    /// 管道被孙进程占着不关时：等到 grace 也得把**已经读到的**交出来。
    ///
    /// 原来这段是 `timeout(...).unwrap_or_default()` —— 默认值是空 `Capture`，
    /// 一条 `nohup x &` 就能让输出凭空消失。
    #[tokio::test]
    async fn captured_output_survives_a_pipe_that_never_closes() {
        use tokio::io::AsyncWriteExt;
        let (mut w, r) = tokio::io::duplex(4096);
        let cap = Arc::new(Mutex::new(Capture::default()));
        let task = tokio::spawn(Capture::read_into(Some(r), None, cap.clone()));
        w.write_all(b"partial output\n").await.unwrap();
        // 写端不关（就像还在跑的后台进程），读任务永远等不到 EOF
        tokio::time::sleep(Duration::from_millis(50)).await;

        let got = drain(task, &cap, Duration::from_millis(100)).await;
        assert!(
            got.head.starts_with(b"partial output\n"),
            "已经读到的输出不能扔：{:?}",
            String::from_utf8_lossy(&got.head)
        );
    }

    /// 失败必须响亮：超时要说明输出是半截的，被杀要说清是哪个信号。
    #[test]
    fn killed_or_timed_out_is_loud() {
        let mut r = ExecResult {
            code: -1,
            stdout: "half\n".into(),
            stderr: String::new(),
            duration_ms: 10,
            truncated: false,
            timed_out: true,
            full_output_path: None,
            using_bash: true,
            signal: Some(9),
        };
        let text = r.to_text();
        assert!(text.contains("超时"), "{text}");
        assert!(text.contains("可能不全"), "超时要说清输出是半截的：{text}");
        assert!(
            !text.contains("被信号"),
            "超时的信号是我们自己发的，别重复报：{text}"
        );
        assert!(text.contains("half"), "输出不能丢：{text}");

        // 不是超时、但被信号带走：要说清是哪个信号、大概为什么
        r.timed_out = false;
        let text = r.to_text();
        assert!(text.contains("被信号 9"), "{text}");
        assert!(text.contains("OOM"), "信号要翻译成人话：{text}");
    }

    /// 没 bash 时结果里必须明说，并给出装它的命令。
    #[test]
    fn missing_bash_is_announced_with_the_fix() {
        let mut r = ExecResult {
            code: 0,
            stdout: "hi\n".into(),
            stderr: String::new(),
            duration_ms: 0,
            truncated: false,
            timed_out: false,
            full_output_path: None,
            using_bash: false,
            signal: None,
        };
        let text = r.to_text();
        assert!(text.contains("/bin/sh"), "{text}");
        assert!(text.contains("agent-setup --force"), "{text}");
        assert!(text.contains("syntax error"), "{text}");

        // 有 bash 时别唠叨
        r.using_bash = true;
        let text = r.to_text();
        assert!(!text.contains("agent-setup"), "{text}");
    }

    #[test]
    fn path_is_explicit_because_the_host_path_is_wrong_inside_the_sandbox() {
        let env = sandbox().guest_env();
        let path = env.iter().find(|(k, _)| k == "PATH").unwrap();
        assert_eq!(path.1, GUEST_PATH);
        assert!(path.1.contains("/bin"));
    }

    #[test]
    fn proot_env_vars_are_injected_because_android_needs_them() {
        // 这三条在安卓上是**必需**的，不是可选优化：
        //  - PROOT_LOADER / _32：不设则 proot 走内嵌 loader 解压路径，
        //    解到 app_data_file 上被 W^X 拒绝 → "'/bin/sh' is not executable"
        //  - PROOT_TMP_DIR：proot 只认这个名字，不认 TMPDIR；
        //    找不到就退回 /tmp，而 App 写不了 /tmp
        let sb = LinuxSandbox::new(SandboxConfig {
            proot: PathBuf::from("/native/libproot.so"),
            rootfs: PathBuf::from("/files/rootfs"),
            binds: vec![],
            tmp: Some(PathBuf::from("/cache/proot-tmp")),
            loader: Some(PathBuf::from("/native/libproot-loader.so")),
            loader32: Some(PathBuf::from("/native/libproot-loader32.so")),
            dns: vec![],
            tz_offset_minutes: None,
            env: BTreeMap::new(),
            workspace: None,
            shared: None,
        });
        let env: BTreeMap<_, _> = sb.guest_env().into_iter().collect();

        assert_eq!(
            env.get("PROOT_LOADER").map(String::as_str),
            Some("/native/libproot-loader.so")
        );
        assert_eq!(
            env.get("PROOT_LOADER_32").map(String::as_str),
            Some("/native/libproot-loader32.so")
        );
        assert_eq!(
            env.get("PROOT_TMP_DIR").map(String::as_str),
            Some("/cache/proot-tmp")
        );
        // 两者是不同的东西，都得有
        assert_eq!(
            env.get("TMPDIR").map(String::as_str),
            Some("/cache/proot-tmp")
        );

        // 沙箱里没有浏览器：$BROWSER 指向 agent-open，沙箱里任何“开浏览器”
        // 的调用（gh auth login / python webbrowser / xdg-open）都会变成
        // “请宿主打开”。
        assert_eq!(
            env.get("BROWSER").map(String::as_str),
            Some("/usr/local/bin/agent-open")
        );
    }

    #[test]
    fn tz_uses_posix_sign_convention() {
        // UTC+8 在 POSIX 里写成 LCL-8（符号相反），否则 musl 解析不出来
        assert_eq!(parse_posix_tz("LCL-8"), Some(480));
        assert_eq!(parse_posix_tz("UTC+5:30"), Some(-330));
        assert_eq!(parse_posix_tz("CST-8"), Some(480));
    }

    fn render_of(n: usize, body: impl Fn(usize) -> String) -> (String, bool) {
        let text: String = (0..n).map(body).collect();
        let mut cap = Capture::default();
        cap.push(text.as_bytes());
        cap.render()
    }

    #[test]
    fn render_keeps_head_and_tail_with_a_marker() {
        // 跨过 2000 行才截断；截断后首尾各 100 行
        let n = MAX_OUTPUT_LINES + 500;
        let (out, truncated) = render_of(n, |i| format!("line {i}\n"));
        assert!(truncated);
        assert!(out.starts_with("line 0\n"), "{out}");
        assert!(out.contains("line 99\n"), "头要留 100 行：{out}");
        assert!(!out.contains("line 100\n"), "第 101 行不该在：{out}");
        assert!(
            out.contains(&format!("[省略 {} 行]", n - HEAD_LINES - TAIL_LINES)),
            "{out}"
        );
        assert!(
            out.contains(&format!("line {}\n", n - 1)),
            "尾要留到最后一行：{out}"
        );
        assert!(!out.contains(&format!("line {}\n", n - 101)), "{out}");
        // 首 100 + 标记 + 尾 100
        assert_eq!(out.lines().count(), HEAD_LINES + TAIL_LINES + 1);
    }

    #[test]
    fn render_of_1999_lines_is_left_alone() {
        // 触发线是 2000 行：差一行不该被省成 200 行，否则模型得反复续读
        let (out, truncated) = render_of(MAX_OUTPUT_LINES - 1, |i| format!("line {i}\n"));
        assert!(!truncated);
        assert!(!out.contains("[省略"), "{out}");
        assert!(out.contains(&format!("line {}\n", MAX_OUTPUT_LINES - 2)));
        assert_eq!(out.lines().count(), MAX_OUTPUT_LINES - 1);
    }

    #[test]
    fn render_of_a_split_capture_shows_both_ends() {
        // 中间被扔掉过（超过内存上限）：头尾分别来自两块缓冲
        let mut cap = Capture::default();
        cap.head.extend_from_slice(b"head-line\n");
        cap.tail.extend_from_slice(b"tail-line\n");
        cap.dropped_lines = 42;
        cap.dropped_bytes = 1234;
        let (out, truncated) = cap.render();
        assert!(truncated);
        assert!(out.starts_with("head-line\n"), "{out}");
        assert!(out.ends_with("tail-line\n"), "{out}");
        assert!(out.contains("[省略 42 行]"), "{out}");
    }

    #[test]
    fn render_shares_the_byte_budget_between_both_ends() {
        // 每行 ~1KB × 300 行：行数没到、字节先超，两侧各分一半预算
        let (out, truncated) = render_of(300, |i| format!("line{i}{}\n", "x".repeat(990)));
        assert!(truncated);
        assert!(out.len() <= MAX_OUTPUT_BYTES, "{} 字节", out.len());
        assert!(!out.starts_with("[省略"), "第一行就得留下：{out}");
        assert!(out.contains("line0"), "{out}");
        assert!(out.contains("line299"), "{out}");
        assert!(out.contains("[省略 "));
    }

    #[test]
    fn render_never_splits_a_multibyte_char() {
        // 一整行 150KB 的中文：两侧只能按字节给半行，不能切出半个字
        let (out, truncated) = render_of(1, |_| "中".repeat(MAX_OUTPUT_BYTES));
        assert!(truncated);
        assert!(out.len() <= MAX_OUTPUT_BYTES, "{} 字节", out.len());
        assert!(!out.contains('\u{fffd}'), "切出了半个字符");
        // 一行都放不下时退回报字节，而不是「省略 0 行」
        assert!(
            out.contains("[省略 ") && !out.contains("[省略 0 行]"),
            "{out}"
        );
    }

    #[test]
    fn render_of_a_few_lines_stays_verbatim() {
        let (out, truncated) = render_of(3, |i| format!("line {i}\n"));
        assert_eq!(out, "line 0\nline 1\nline 2\n");
        assert!(!truncated);
        // 空输出还是空
        assert_eq!(Capture::default().render(), (String::new(), false));
    }

    #[test]
    fn spilled_file_marks_the_gap_it_could_not_keep() {
        // 中间扔过东西 → 文件里也要断开，不能看着像连着的
        let mut cap = Capture::default();
        cap.head.extend_from_slice(b"head\n");
        cap.tail.extend_from_slice(b"tail\n");
        cap.dropped_bytes = 4 << 20;
        let mut body = Vec::new();
        cap.write_to(&mut body);
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains("中间省略 4.0MB"), "{body}");

        // 没扔过东西（总量不到上限）就不该凭空出现省略提示
        let mut cap = Capture::default();
        cap.push(b"hello\nworld\n");
        let mut body = Vec::new();
        cap.write_to(&mut body);
        assert_eq!(String::from_utf8(body).unwrap(), "hello\nworld\n");
    }

    // --- 路径解析 ---------------------------------------------------------

    struct TempRoot {
        dir: PathBuf,
    }

    impl TempRoot {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "agent-linux-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            std::fs::create_dir_all(dir.join("root/sub")).unwrap();
            std::fs::create_dir_all(dir.join("etc")).unwrap();
            std::fs::create_dir_all(dir.join("tmp")).unwrap();
            // 必须是普通文件，否则 is_alpine_rootfs 认不出来
            std::fs::write(dir.join("bin/busybox"), b"").unwrap();
            Self { dir }
        }

        fn sandbox(&self) -> LinuxSandbox {
            LinuxSandbox::new(SandboxConfig {
                proot: PathBuf::from("proot"),
                rootfs: self.dir.clone(),
                binds: vec![],
                tmp: None,
                dns: vec!["1.1.1.1".into()],
                tz_offset_minutes: None,
                loader: None,
                loader32: None,
                env: BTreeMap::new(),
                workspace: None,
                shared: None,
            })
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn absolute_and_relative_paths_both_land_inside_the_rootfs() {
        let t = TempRoot::new("paths");
        let sb = t.sandbox();

        assert_eq!(
            sb.resolve_path("/root/a.txt").unwrap(),
            t.dir.join("root/a.txt")
        );
        // 相对路径以 /workspace 为 cwd（没挂手机存储时就是 rootfs 里的目录）
        assert_eq!(
            sb.resolve_path("a.txt").unwrap(),
            t.dir.join("workspace/a.txt")
        );
        assert_eq!(
            sb.resolve_path("/etc/resolv.conf").unwrap(),
            t.dir.join("etc/resolv.conf")
        );
    }

    #[test]
    fn path_traversal_never_leaves_the_rootfs() {
        let t = TempRoot::new("escape");
        let sb = t.sandbox();

        // 先在沙箱视角里归一化：和真实 Linux 一样，根目录的 `..` 还是根目录。
        // 所以这些路径要么被拒，要么落在 rootfs 里 —— 绝不会到宿主机上去
        for bad in [
            "/../outside.txt",
            "/root/../../outside.txt",
            "../../outside.txt",
            "/root/../..",
        ] {
            if let Ok(p) = sb.resolve_path(bad) {
                assert!(
                    p.starts_with(&t.dir),
                    "{bad} 解析到了 rootfs 外面: {}",
                    p.display()
                );
            }
        }
    }

    #[test]
    fn workspace_and_phone_storage_are_mounted_and_confined() {
        let t = TempRoot::new("mounts");
        let phone = t.dir.with_extension("phone");
        let _ = std::fs::remove_dir_all(&phone);
        std::fs::create_dir_all(&phone).unwrap();

        let cfg = t
            .sandbox()
            .config()
            .clone()
            .with_shared_storage(Some(&phone));
        assert!(phone.join("Agent").is_dir(), "应该在手机存储里建好 Agent/");
        let sb = LinuxSandbox::new(cfg);
        assert!(sb.has_shared_storage());

        let ws = phone.join("Agent");
        assert_eq!(
            sb.resolve_path("/workspace/a/b.html").unwrap(),
            ws.join("a/b.html")
        );
        assert_eq!(
            sb.resolve_path("b.html").unwrap(),
            ws.join("b.html"),
            "相对路径落在工作目录"
        );
        assert_eq!(
            sb.resolve_path("/sdcard/DCIM/x.jpg").unwrap(),
            phone.join("DCIM/x.jpg")
        );
        // 想从挂载点 .. 出去：在沙箱视角里归一化后回到 rootfs，到不了宿主机
        assert_eq!(
            sb.resolve_path("/sdcard/../etc/hosts").unwrap(),
            t.dir.join("etc/hosts")
        );
        assert_eq!(
            sb.resolve_path("/workspace/../../x").unwrap(),
            t.dir.join("x")
        );

        let argv = sb.command_line("true");
        let joined = argv.join(" ");
        assert!(joined.contains(&format!("-b {}:/sdcard", phone.display())));
        assert!(joined.contains(&format!("-b {}:/workspace", ws.display())));

        // 断开（权限被收回）：回到 rootfs 里的 /workspace
        let off = LinuxSandbox::new(sb.config().clone().with_shared_storage(None));
        assert!(!off.has_shared_storage());
        assert_eq!(
            off.resolve_path("b.html").unwrap(),
            t.dir.join("workspace/b.html")
        );

        let _ = std::fs::remove_dir_all(&phone);
    }

    #[test]
    fn symlinks_pointing_outside_the_rootfs_are_rejected() {
        let t = TempRoot::new("symlink");
        let sb = t.sandbox();

        // 在 rootfs 里放一个指向宿主机 /etc/passwd 的链接
        std::os::unix::fs::symlink("/etc/passwd", t.dir.join("root/evil")).unwrap();
        assert!(
            sb.resolve_path("/root/evil").is_err(),
            "符号链接逃逸没被拦住"
        );
    }

    // --- 文件编辑 ---------------------------------------------------------

    #[test]
    fn edit_file_reads_applies_and_writes_back() {
        let t = TempRoot::new("edit");
        let sb = t.sandbox();
        std::fs::write(t.dir.join("root/conf.txt"), "host=old\nport=80\n").unwrap();

        let out = sb
            .edit_file(
                "/root/conf.txt",
                &[
                    Edit::new("host=old", "host=new"),
                    Edit::new("port=80", "port=443"),
                ],
            )
            .unwrap();

        assert_eq!(
            sb.read_file("/root/conf.txt").unwrap(),
            "host=new\nport=443\n"
        );
        assert_eq!(out.first_changed_line, 1);
        assert!(out.patch.contains("-host=old"));
        assert!(out.patch.contains("+host=new"));
    }

    /// 写文件是「临时文件 + rename」：改完不留临时文件、保留可执行位；
    /// 改一个符号链接时改的是它指向的文件，链接本身还是链接。
    #[test]
    fn edit_writes_atomically_keeping_mode_and_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempRoot::new("atomic");
        let sb = t.sandbox();
        let script = t.dir.join("root/run.sh");
        std::fs::write(&script, "echo old\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("run.sh", t.dir.join("root/link.sh")).unwrap();

        sb.edit_file("/root/link.sh", &[Edit::new("old", "new")])
            .unwrap();

        assert_eq!(std::fs::read_to_string(&script).unwrap(), "echo new\n");
        let link = std::fs::symlink_metadata(t.dir.join("root/link.sh")).unwrap();
        assert!(link.file_type().is_symlink(), "链接不能被换成普通文件");
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "可执行位要保住");
        let leftovers: Vec<_> = std::fs::read_dir(t.dir.join("root"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件要么被 rename 掉，要么清掉");
    }

    #[test]
    fn edit_file_refuses_a_missing_file_with_a_useful_message() {
        let t = TempRoot::new("editmissing");
        let err = t
            .sandbox()
            .edit_file("/root/nope.txt", &[Edit::new("a", "b")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("不存在"), "{err}");
        assert!(
            err.contains("content"),
            "错误信息应该告诉模型怎么新建: {err}"
        );
    }

    #[test]
    fn edit_file_rejects_ambiguous_edits_without_touching_disk() {
        let t = TempRoot::new("editdup");
        let sb = t.sandbox();
        let original = "x\nx\n";
        std::fs::write(t.dir.join("root/f.txt"), original).unwrap();

        assert!(sb.edit_file("/root/f.txt", &[Edit::new("x", "y")]).is_err());
        // 失败时不能留下半成品
        assert_eq!(sb.read_file("/root/f.txt").unwrap(), original);
    }

    // --- 文件读取 ---------------------------------------------------------

    #[test]
    fn read_text_resolves_the_path_and_applies_offset_and_limit() {
        let t = TempRoot::new("read");
        let sb = t.sandbox();
        std::fs::create_dir_all(t.dir.join("workspace")).unwrap();
        std::fs::write(t.dir.join("workspace/a.txt"), "l0\nl1\nl2\nl3\n").unwrap();

        // 相对路径按 cwd `/workspace` 算
        let v = sb.read_text("a.txt", 2, Some(2), false).unwrap();
        assert!(v.content.starts_with("l1\nl2"), "{}", v.content);
        assert!(v.content.contains("offset=4"), "{}", v.content);
    }

    #[test]
    fn oversized_text_files_are_refused_before_reading() {
        assert!(check_text_size(1024, "/root/a.txt").is_ok());
        assert!(check_text_size(MAX_TEXT_BYTES, "/root/a.txt").is_ok());
        let err = check_text_size(MAX_TEXT_BYTES + 1, "/root/big.log")
            .unwrap_err()
            .to_string();
        assert!(err.contains("OOM"), "{err}");
        assert!(err.contains("sed -n"), "要给出替代做法: {err}");
        assert!(err.contains("/root/big.log"), "{err}");
    }

    #[test]
    fn read_text_rejects_missing_files_and_binaries_with_a_useful_message() {
        let t = TempRoot::new("readbad");
        let sb = t.sandbox();

        let err = sb
            .read_text("/root/nope.txt", 1, None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("不存在"), "{err}");
        assert!(err.contains("bash"), "应该告诉模型替代做法: {err}");

        std::fs::write(t.dir.join("root/blob.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let err = sb
            .read_text("/root/blob.bin", 1, None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("UTF-8"), "{err}");
    }

    // --- edit 的 create 模式 ---------------------------------------------

    #[test]
    fn create_file_writes_new_files_and_makes_parent_dirs() {
        let t = TempRoot::new("create");
        let sb = t.sandbox();

        // 相对路径按 /workspace 算；父目录不存在也得能建（模型常写 a/b/c.py）
        let made = sb.create_file("sub/dir/a.py", "print('hi')\n").unwrap();
        assert_eq!((made.lines, made.bytes), (1, 12));
        assert_eq!(
            std::fs::read_to_string(t.dir.join("workspace/sub/dir/a.py")).unwrap(),
            "print('hi')\n"
        );

        // 空文件也算建出来
        let made = sb.create_file("/root/empty", "").unwrap();
        assert_eq!((made.lines, made.bytes), (0, 0));
        assert!(t.dir.join("root/empty").is_file());
    }

    #[test]
    fn create_file_refuses_to_overwrite_and_says_what_to_do() {
        let t = TempRoot::new("createexists");
        let sb = t.sandbox();
        std::fs::write(t.dir.join("root/a.txt"), "原文").unwrap();

        let err = sb
            .create_file("/root/a.txt", "盖掉")
            .unwrap_err()
            .to_string();
        assert!(err.contains("已存在"), "{err}");
        assert!(err.contains("edits"), "要告诉模型怎么改：{err}");
        // 原文一个字都不能动
        assert_eq!(
            std::fs::read_to_string(t.dir.join("root/a.txt")).unwrap(),
            "原文"
        );
    }

    #[test]
    fn create_file_refuses_to_escape_through_a_symlink_or_dotdot() {
        let t = TempRoot::new("createlink");
        let sb = t.sandbox();
        let outside = std::env::temp_dir().join(format!("agent-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();

        // 词法越界（`..`）到不了外面：沙箱视角里 `/root/../..` 就是 `/`
        sb.create_file("/root/../../escape.txt", "x").unwrap();
        assert!(
            t.dir.join("escape.txt").is_file(),
            "`..` 应该被夹在 rootfs 里，落到 <rootfs>/escape.txt"
        );

        // 符号链接越界：模型能在沙箱里 `ln -s <外面> /workspace/link`，
        // 而我们是在宿主上按路径写的 —— 必须往上找真实路径查一遍
        std::fs::create_dir_all(t.dir.join("workspace")).unwrap();
        std::os::unix::fs::symlink(&outside, t.dir.join("workspace/link")).unwrap();
        let err = sb
            .create_file("/workspace/link/evil.txt", "x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("沙箱外"), "{err}");
        assert!(!outside.join("evil.txt").exists(), "不能写到沙箱外面去");

        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn prepare_rootfs_writes_resolv_conf_because_apk_needs_it() {
        let dir = std::env::temp_dir().join(format!("agent-linux-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        // 必须是**普通文件**才算 rootfs —— bin/sh 是绝对符号链接，宿主机解析不到
        std::fs::write(dir.join("bin/busybox"), b"").unwrap();

        let sb = LinuxSandbox::new(SandboxConfig {
            proot: PathBuf::from("proot"),
            rootfs: dir.clone(),
            binds: vec![],
            tmp: None,
            dns: vec!["9.9.9.9".into(), "1.1.1.1".into()],
            tz_offset_minutes: None,
            loader: None,
            loader32: None,
            env: BTreeMap::new(),
            workspace: None,
            shared: None,
        });
        sb.prepare_rootfs().unwrap();

        let resolv = std::fs::read_to_string(dir.join("etc/resolv.conf")).unwrap();
        assert_eq!(resolv, "nameserver 9.9.9.9\nnameserver 1.1.1.1\n");
        assert!(dir.join("root").is_dir());
        assert!(dir.join("tmp").is_dir());

        // 幂等
        sb.prepare_rootfs().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 沙箱里那个 `agent` 命令是 `prepare_rootfs` 写进去的。
    ///
    /// 它是模型「查看 / 操作自己」的唯一入口（`agent state` / `agent request` /
    /// `agent command`），掉了就等于这个能力没了 —— 而且不会有任何报错，
    /// 只是模型发现 `agent: not found`。
    #[test]
    fn prepare_rootfs_installs_the_agent_cli() {
        let t = TempRoot::new("agentcli");
        let sb = t.sandbox();
        sb.prepare_rootfs().unwrap();

        let cli = t.dir.join("usr/local/bin/agent");
        let body = std::fs::read_to_string(&cli).expect("agent 命令没写进去");
        assert!(body.starts_with("#!/bin/sh"), "要有 shebang");
        assert!(body.contains("/api/debug/request"), "得能看自己的上下文");
        assert!(body.contains("/api/command"), "得能操作手机");
        assert!(body.contains("AGENT_PORT"), "端口从环境变量来");
        // 浏览器现在是**唯一的门**（`browser` 工具删了），多标签那几个子命令
        // 得在 —— 它们曾经只加在工具那边，CLI 没跟上，于是 `new_tab URL`
        // 走进兜底分支把 URL 静默吃掉。
        for sub in ["navigate", "new_tab", "list_tabs", "switch_tab", "close_tab"] {
            assert!(body.contains(sub), "浏览器子命令 {sub} 掉了");
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cli).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "必须可执行（现在是 {:o}）", mode);
        }

        // 语法交给 sh -n 过一遍（busybox ash 的语法是 POSIX 子集，够了）
        if let Ok(out) = std::process::Command::new("sh")
            .arg("-n")
            .arg(&cli)
            .output()
        {
            assert!(
                out.status.success(),
                "shell 语法不过：{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        // 幂等：再跑一次不炸
        sb.prepare_rootfs().unwrap();
        let _ = std::fs::remove_dir_all(&t.dir);
    }

    /// `$var` 后面**不能**紧跟中文标点，必须写 `${var}`。
    ///
    /// 沙箱里是 busybox ash，它不认 UTF-8：把它后面的全角字节当成变量名的
    /// 一部分，于是 `$cmd（` 变成一个叫 `cmd（` 的变量、`set -u` 下
    /// 「unbound variable」退出 1 —— 那句提示语永远打不出来（`agent frobnicate`
    /// 就这么稢了很久，直到给 `agent browser` 加参数校验时才被碰出来）。
    #[test]
    fn agent_cli_braces_variables_before_multibyte() {
        let b = AGENT_CLI.as_bytes();
        let name = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        let mut i = 0usize;
        let mut bad: Vec<String> = Vec::new();
        while i < b.len() {
            let start_ok = b[i] == b'$'
                && i + 1 < b.len()
                && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'_');
            if start_ok {
                let mut j = i + 1;
                while j < b.len() && name(b[j]) {
                    j += 1;
                }
                if j < b.len() && b[j] >= 0x80 {
                    bad.push(String::from_utf8_lossy(&b[i..j]).into_owned());
                }
                i = j;
                continue;
            }
            i += 1;
        }
        assert!(
            bad.is_empty(),
            "这些变量后面紧跟多字节字符，沙箱的 ash 会把它当成变量名的一部分：{bad:?}"
        );
    }

    /// 预装脚本也得写进去、可执行，而且**工具清单不能是空的**。
    ///
    /// 这个清单掉了不会有任何报错 —— 只是模型又开始每个回合自己 `apk add`。
    #[test]
    fn prepare_rootfs_installs_the_toolset_script() {
        let t = TempRoot::new("agentsetup");
        let sb = t.sandbox();
        sb.prepare_rootfs().unwrap();

        let sh = t.dir.join("usr/local/bin/agent-setup");
        let body = std::fs::read_to_string(&sh).expect("agent-setup 没写进去");
        assert!(body.starts_with("#!/bin/sh"));
        for tool in TOOLSET {
            assert!(body.contains(tool), "清单里的 {tool} 没出现在脚本里");
        }
        assert!(!body.contains("__PACKAGES__"), "模板占位没替换");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&sh).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "必须可执行");
        }
        if let Ok(out) = std::process::Command::new("sh").arg("-n").arg(&sh).output() {
            assert!(
                out.status.success(),
                "shell 语法不过：{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        // 清单得真管用：模型最常碰的几样必须在
        for want in [
            "ripgrep",
            "fd",
            "jq",
            "python3",
            "curl",
            "bash",
            "github-cli",
        ] {
            assert!(TOOLSET.contains(&want), "预装清单里少了 {want}");
        }

        // 没装过 → false；标记文件版本对上 → true；**版本对不上要重新装**
        assert!(!sb.is_provisioned());
        std::fs::write(t.dir.join("etc/agent-tools"), "1").unwrap();
        assert!(!sb.is_provisioned(), "老版本的标记不该算装过");
        std::fs::write(t.dir.join("etc/agent-tools"), TOOLSET.len().to_string()).unwrap();
        assert!(sb.is_provisioned());

        let _ = std::fs::remove_dir_all(&t.dir);
    }

    /// 沙箱里没有浏览器：`$BROWSER` 和 xdg-open 这些名字都指向 `agent-open`，
    /// 它只把网址打回输出 —— 由 agent 在回复里转告用户（没有“自动打开”这回事）。
    /// 另外这六个都是“开浏览器”的常见名字，至少别让调用方拿到 not found。
    #[test]
    fn rootfs_gets_the_browser_shims() {
        let t = TempRoot::new("open");
        let sb = t.sandbox();
        sb.prepare_rootfs().unwrap();

        let open = t.dir.join("usr/local/bin/agent-open");
        let body = std::fs::read_to_string(&open).expect("agent-open 没写进去");
        assert!(body.starts_with("#!/bin/sh"));
        // 只剩一个薄壳：真正的逻辑在 `agent open` 里
        assert!(body.contains("exec /usr/local/bin/agent open"), "{body}");

        for alias in [
            "xdg-open",
            "sensible-browser",
            "www-browser",
            "x-www-browser",
            "gnome-open",
            "kde-open",
        ] {
            let p = t.dir.join("usr/local/bin").join(alias);
            let shim =
                std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{alias} 没写进去: {e}"));
            assert!(
                shim.contains("exec /usr/local/bin/agent-open"),
                "{alias}: {shim}"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&p).unwrap().permissions().mode();
                assert_eq!(mode & 0o111, 0o111, "{alias} 必须可执行");
            }
        }

        if let Ok(out) = std::process::Command::new("sh")
            .arg("-n")
            .arg(&open)
            .output()
        {
            assert!(
                out.status.success(),
                "agent-open 语法不过：{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let _ = std::fs::remove_dir_all(&t.dir);
    }

    /// `agent` 命令靠 `AGENT_PORT` 找到核心；核心还没 bind 时不该注入。
    #[test]
    fn guest_env_carries_the_agent_port_once_known() {
        let t = TempRoot::new("agentport");
        let sb = t.sandbox();

        let has = |k: &str| sb.guest_env().iter().any(|(n, _)| n == k);
        assert!(!has("AGENT_PORT"), "还没 bind 就不该有端口");

        sb.set_agent_port(41234);
        let env: BTreeMap<String, String> = sb.guest_env().into_iter().collect();
        assert_eq!(env.get("AGENT_PORT").map(String::as_str), Some("41234"));
        assert_eq!(
            env.get("AGENT_BASE").map(String::as_str),
            Some("http://127.0.0.1:41234")
        );

        let _ = std::fs::remove_dir_all(&t.dir);
    }

    #[test]
    fn alpine_detection_ignores_symlinks_to_absolute_paths() {
        let dir = std::env::temp_dir().join(format!("agent-linux-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();

        // 模拟 Alpine：bin/sh 是指向绝对路径的符号链接。
        // 用 Path::exists() 判断的话，它会拿宿主机的 /bin/busybox 去解析，
        // 在宿主上没有 → 误判成"不是 rootfs"。
        std::os::unix::fs::symlink("/bin/busybox", dir.join("bin/sh")).unwrap();
        assert!(!is_alpine_rootfs(&dir), "只靠 bin/sh 会误判");

        std::fs::write(dir.join("bin/busybox"), b"").unwrap();
        assert!(is_alpine_rootfs(&dir));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
