//! 沙箱 / 宿主里的 `agent` 命令：一张子命令表，**按机器生成**一份脚本。
//!
//! 为什么要有这个命令：模型平时只看得见自己的上下文，看不见这台机器上还能做什么。
//! 浏览器、联网搜索、天气、手机的系统能力……都从这一个门进（不开成工具：一条 bash
//! 能把几步连起来，工具一次只做一件事；见 docs/10）。
//!
//! ## 为什么按机器生成，而不是一份脚本里运行时判断
//!
//! 三种机器能做的事不一样：
//!
//! - [`Machine::Phone`]：手机（proot 沙箱 + 安卓）。设备操作、系统能力都有。
//! - [`Machine::Sandbox`]：电脑上的 proot 沙箱（`just dev`）。形态和手机一样，
//!   但没有设备、没有闹钟短信。
//! - [`Machine::Host`]：电脑宿主模式（没有沙箱）。进程、装包都用系统自己的。
//!
//! 以前是一份脚本靠环境变量在运行时开关，于是模型在电脑上读到「操作这台手机（只有
//! 手机上有）」、在手机上读到「这台电脑上没有它们」—— 它不该看到这层区分。现在每台
//! 机器写进去的 `agent` 里**只有**这台机器有的子命令：help、分发、提示词里那份清单
//! （[`prompt_clause`]）都从同一张表（[`subcommands`]）按机器取。
//!
//! 只用 busybox 的 wget 或者 curl，没有别的依赖 —— Alpine minirootfs 里自带 wget；
//! 宿主上一般是 curl。端口和凭据由核心通过 `AGENT_PORT` / `AGENT_TOKEN` 注入。
//!
//! ⚠️ 脚本里变量后面紧跟中文标点必须写 `${var}`：沙箱里是 busybox ash，它不认
//! UTF-8，会把全角字节当成变量名的一部分，`set -u` 下直接「unbound variable」退出 1
//! （有测试守着：`variables_are_braced_before_multibyte`）。

use crate::platform::Platform;

/// `agent` 命令跑在哪种机器上（见模块说明）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Machine {
    Phone,
    Sandbox,
    Host,
}

impl Machine {
    pub const ALL: [Machine; 3] = [Machine::Phone, Machine::Sandbox, Machine::Host];

    /// 提示词那边的 (平台, 有没有手机能力) → 机器。
    pub fn of(p: Platform, phone: bool) -> Self {
        match (p, phone) {
            (Platform::Android, true) => Machine::Phone,
            (Platform::Android, false) => Machine::Sandbox,
            (Platform::Linux | Platform::Macos, _) => Machine::Host,
        }
    }

    /// 写进 proot 沙箱的那份是哪种机器（手机 or 电脑上的 `just dev`）。
    pub fn sandbox_here() -> Self {
        if crate::linux::has_phone_capabilities() {
            Machine::Phone
        } else {
            Machine::Sandbox
        }
    }

    /// `agent browser` 背后是什么（提示词里那一句）
    fn browser(self) -> &'static str {
        match self {
            Machine::Phone => "真 WebView",
            Machine::Sandbox | Machine::Host => "无头 Chrome",
        }
    }
}

const ALL: &[Machine] = &Machine::ALL;
const PROOT: &[Machine] = &[Machine::Phone, Machine::Sandbox];
const PHONE: &[Machine] = &[Machine::Phone];
const DESKTOP: &[Machine] = &[Machine::Sandbox, Machine::Host];

/// 一个子命令。
struct Sub {
    /// `case` 里的模式（一个名字，不设别名 —— 别名只会让 help 和提示词各说各的）
    name: String,
    /// `agent help` 里的那几行（每行自带 `agent ` 开头和缩进）
    help: String,
    /// 提示词里那一项（空 = 不进提示词）。`{browser}` 换成这台机器的浏览器。
    prompt: &'static str,
    /// 在哪些机器上有
    on: &'static [Machine],
    /// `case` 分支体（4 格缩进，不含 `;;`）
    body: String,
}

fn sub(
    name: &str,
    on: &'static [Machine],
    prompt: &'static str,
    help: &str,
    body: &str,
) -> Sub {
    Sub {
        name: name.to_string(),
        help: help.to_string(),
        prompt,
        on,
        body: body.to_string(),
    }
}

/// 全部子命令，**顺序就是 `agent help` 和提示词里的顺序**。
fn subcommands() -> Vec<Sub> {
    let mut v = vec![
        // 浏览器和搜索在提示词里给出完整签名：这两个最常用，而且不知道参数就没法用 ——
        // 只给名字的话，模型几乎每次都得先花一轮跑 `agent help`。别的命令只给名字
        //（提示词越长，升级时追加进老会话的那份也越长，见 `agent::ensure_system_prompt`）。
        sub(
            "browser",
            ALL,
            "`agent browser <动作> [参数…]`（{browser}；动作：{actions}；一条命令里可以用 && 连做多步）",
            &browser_help(),
            BROWSER_BODY,
        ),
        sub(
            "search",
            ALL,
            "`agent search <检索意图> [条数]`（联网搜索，回标题 + 链接 + 片段）",
            "  agent search <检索意图> [条数]      联网搜索，回标题 + 链接 + 片段（排好版的文字）",
            SEARCH_BODY,
        ),
        sub(
            "weather",
            ALL,
            "",
            "  agent weather [current|hourly|daily|report] --lat L --lon N   天气（Open-Meteo）\n\
             \x20   --lat/--lon 是十进制度（纬度 -90..90，经度 -180..180）",
            WEATHER_BODY,
        ),
        sub(
            "open",
            PHONE,
            "`agent open <uri>`（打开网址 / App 深链）",
            "  agent open <uri>                    打开它：http(s) / App 深链（weixin://、androidamap://）\n\
             \x20   / intent:// / tel: …（沙箱里的 $BROWSER / xdg-open 最后也走到它）",
            OPEN_BODY,
        ),
        sub(
            "open",
            DESKTOP,
            "`agent open <uri>`（用这台电脑的默认程序打开网页/文件）",
            "  agent open <uri>                    用这台电脑的默认程序打开网址或文件",
            OPEN_BODY,
        ),
        sub(
            "key",
            PHONE,
            "`agent key|tap|swipe|launch`（按键/点击/滑动/启动应用）",
            "  agent key back|home|recents|notifications   按系统键",
            KEY_BODY,
        ),
        sub(
            "tap",
            PHONE,
            "",
            "  agent tap <x> <y>                   点屏幕上的一个点（像素坐标）",
            TAP_BODY,
        ),
        sub(
            "swipe",
            PHONE,
            "",
            "  agent swipe <x1> <y1> <x2> <y2> [毫秒]   从一点滑到另一点（默认 300 毫秒）",
            SWIPE_BODY,
        ),
        sub(
            "launch",
            PHONE,
            "",
            "  agent launch <包名>                 启动应用（只到首页；到具体页面用 agent open 深链）",
            LAUNCH_BODY,
        ),
    ];

    // 手机的系统能力：一个 case 分支（名字从 `phone::COMMANDS` 来），help 一条一行
    let names = crate::phone::names();
    let mut help: Vec<String> = crate::phone::COMMANDS
        .iter()
        .map(|c| format!("  agent {}", c.usage.replace('\n', "\n  ")))
        .collect();
    help.push(
        "  （每个都有 --help；第一次用联系人 / 日历 / 短信 / 定位会弹系统权限框，人在旁边才点得动）"
            .into(),
    );
    v.push(Sub {
        name: names.join("|"),
        help: help.join("\n"),
        prompt: "",
        on: PHONE,
        body: PHONE_BODY.to_string(),
    });

    v.extend([
        // 提示词里那一句是动态的（接了哪几个服务），不在这张表里：见 `mcp::prompt_line`
        sub(
            "mcp",
            ALL,
            "",
            "  agent mcp list [服务]               已接的 MCP 服务和它们的工具\n\
             \x20   agent mcp <服务> <工具> --help      这个工具的说明和参数\n\
             \x20   agent mcp <服务> <工具> ['<json参数>']   调用",
            MCP_BODY,
        ),
        sub(
            "files",
            PROOT,
            "",
            "  agent files [路径]                  看工作目录（默认 /workspace）成什么样了：哪些是项目目录、\n\
             \x20   哪些文件散在根目录下（只报告，不动文件）",
            FILES_BODY,
        ),
        sub(
            "setup",
            PROOT,
            "",
            "  agent setup [--force|--list|包…]    装/看常用工具（rg fd jq git python3…）",
            SETUP_BODY,
        ),
        sub(
            "ps",
            PROOT,
            "`agent ps` / `agent kill <pid>`（看/停还在跑的进程 —— 包括你自己用 `setsid` 放后台的常驻服务）",
            "  agent ps                            还有什么在跑（包括命令结束后仍活着的常驻进程）",
            PS_BODY,
        ),
        sub(
            "kill",
            PROOT,
            "",
            "  agent kill <pid>                    停掉一个（pid 从 agent ps 来）",
            KILL_BODY,
        ),
        sub(
            "debug",
            ALL,
            "",
            "  agent debug <state|request|tools|sessions|session|doctor>   看自己的内部状态（调试用）\n\
             \x20   agent debug get <path> / post <path> <json>   直接调自己的接口",
            DEBUG_BODY,
        ),
    ]);
    v
}

/// 这台机器的 `agent` 脚本。
pub fn script(m: Machine) -> String {
    let subs: Vec<Sub> = subcommands()
        .into_iter()
        .filter(|s| s.on.contains(&m))
        .collect();
    let mut out = String::from(PRELUDE);
    out.push_str("cmd=\"${1:-help}\"\ncase \"$cmd\" in\n");
    for s in &subs {
        out.push_str(&format!("  {})\n{}\n    ;;\n", s.name, s.body.trim_end()));
    }
    out.push_str("  help|-h|--help)\n    cat <<'EOT'\n");
    out.push_str(&help_text(&subs));
    out.push_str("\nEOT\n    ;;\n");
    out.push_str(
        "  *)\n    echo \"不认识的子命令：${cmd}（agent help 看全部）\" >&2\n    exit 2\n    ;;\nesac\n",
    );
    out
}

fn help_text(subs: &[Sub]) -> String {
    let mut lines = vec!["agent <命令> [参数…]".to_string(), String::new()];
    lines.extend(subs.iter().map(|s| s.help.clone()));
    lines.push(String::new());
    lines.push("参数给错了会报错，不会静默忽略。".into());
    lines.join("\n")
}

/// 提示词里那份清单（`agent browser`（…）、`agent search`（…）、…）。
pub fn prompt_clause(m: Machine) -> String {
    let mut items: Vec<String> = subcommands()
        .into_iter()
        .filter(|s| s.on.contains(&m) && !s.prompt.is_empty())
        .map(|s| {
            s.prompt
                .replace("{browser}", m.browser())
                .replace("{actions}", &BROWSER_ACTIONS.join(" | "))
        })
        .collect();
    // 系统能力那一组：一项列全名（顺序和 help 一致），放在设备操作后面
    if m == Machine::Phone {
        let at = items
            .iter()
            .position(|i| i.starts_with("`agent key"))
            .map_or(items.len(), |i| i + 1);
        items.insert(
            at,
            format!("`agent {}`（这台手机的系统能力）", crate::phone::names().join("|")),
        );
    }
    items.join("、")
}

// ---------------------------------------------------------------------------
// 脚本片段
// ---------------------------------------------------------------------------

const PRELUDE: &str = r##"#!/bin/sh
# agent —— 由核心按这台机器生成（core/src/cli.rs），别手改。`agent help` 看用法。
set -eu

BASE="${AGENT_BASE:-http://127.0.0.1:${AGENT_PORT:-}}"
if [ -z "$BASE" ] || [ "$BASE" = "http://127.0.0.1:" ]; then
  echo "AGENT_PORT 没设 —— 不是在 agent 的 bash 里跑？" >&2
  exit 1
fi
# 核心的接口要凭据（手机上别的 App 也能连 127.0.0.1）。核心通过环境变量给。
AUTH="x-agent-token: ${AGENT_TOKEN:-}"

# 「改自己身份」的接口不从这儿走：别让网页里的一段话把 agent 自己改了。
guard() {
  case "$1" in
    /api/llm*|/api/debug/system-prompt*)
      echo "拒绝：$1 会改掉我自己的身份（模型 / key / 提示词）。" >&2
      echo "那两件事留给人做：在宿主机上跑 scripts/agentctl.py" >&2
      exit 3
      ;;
    # 接哪些 MCP 服务、带什么凭据由人在设置里定：网页里一段话不该能让我把调用转给别人的服务
    /api/mcp/servers*)
      echo "拒绝：MCP 服务由人在设置里配。" >&2
      exit 3
      ;;
  esac
}

# 核心回的内容不带结尾换行：补一个，免得 `agent a && agent b` 的两段输出粘成一行
get() {
  guard "$1"
  if command -v curl >/dev/null 2>&1; then
    curl -sS -m 120 -H "$AUTH" "$BASE$1" || { echo "GET $1 失败" >&2; exit 1; }
  else
    wget -q -T 120 -O - --header "$AUTH" "$BASE$1" || { echo "GET $1 失败" >&2; exit 1; }
  fi
  echo
}

post() {
  guard "$1"
  if command -v curl >/dev/null 2>&1; then
    curl -sS -m 120 -H "$AUTH" -H 'content-type: application/json' -d "$2" "$BASE$1" \
      || { echo "POST $1 失败" >&2; exit 1; }
  else
    wget -q -T 120 -O - --header "$AUTH" --header 'content-type: application/json' --post-data "$2" "$BASE$1" \
      || { echo "POST $1 失败" >&2; exit 1; }
  fi
  echo
}

# 把一行 JSON 摊开，方便看（没有 jq，纯 shell）
fmt() { tr ',' '\n' | tr -d '{}"' | sed 's/^[[:space:]]*//; /^[[:space:]]*$/d'; }

# 参数里的反斜杠和引号要先转义，否则拼出来的 JSON 是坏的
esc() { printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'; }

# 把参数变成一个 JSON 数组（argv 形状）
json_args() {
  _out=""
  for _a in "$@"; do
    [ -z "$_out" ] || _out="$_out,"
    _out="$_out\"$(esc "$_a")\""
  done
  printf '[%s]' "$_out"
}

# 纯数字参数：非数字直接报错，别拼出一段坏 JSON 给核心
int_arg() { case "${1:-}" in ''|*[!0-9]*) echo "agent ${2}：要一个数字，给的是「${1:-}」" >&2; exit 2 ;; esac; }

# 设备操作（按键 / 点击 / 滑动 / 启动）：发给核心，失败变成非 0 退出码 ——
# 模型常写 `agent tap … && …` 这种链，回了 0 它会当成成功接着往下跑
act() {
  _r=$(post /api/command "{\"cmd\":$1}")
  printf '%s\n' "$_r"
  case "$_r" in *'"ok":true'*) ;; *) exit 1 ;; esac
}

"##;

/// `agent browser` 的动作和参数（help 和提示词共用这一份）。
const BROWSER_ACTIONS: &[&str] = &[
    "navigate <url>",
    "get_backbone [深度]",
    "click <选择器>",
    "type <选择器> <文本>",
    "get_text [选择器]",
    "get_readable",
    "scroll [up|down] [像素]",
    "screenshot [true=整页]",
    "press <键名>",
    "find_elements <选择器>",
    "user_agent mobile|desktop",
    "back",
    "wait_for_dom_stable [秒]",
    "js '<代码>'",
    "new_tab [url]",
    "list_tabs",
    "switch_tab <编号>",
    "close_tab [编号]",
];

fn browser_help() -> String {
    let mut lines =
        vec!["  agent browser <动作> [参数…]        驱动浏览器（agent browser --help 看每个动作）：".to_string()];
    lines.extend(
        BROWSER_ACTIONS
            .chunks(4)
            .map(|c| format!("      {}", c.join(" | "))),
    );
    lines.extend([
        "    一条命令里可以连做多步：agent browser navigate <url> && agent browser get_backbone 3".into(),
        "    截图路径在 screenshot 的返回里，要 read 它才看得见。".into(),
        "    复杂参数直接给 JSON：agent browser '{\"action\":\"click\",\"x\":100,\"y\":200}'".into(),
    ]);
    lines.join("\n")
}

const BROWSER_BODY: &str = r##"    shift || true
    act_="${1:-}"; shift || true
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
  screenshot [true]                截图（true = 整页）；路径在返回里，用 read 看
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
    case "$act_" in
      help|-h|--help) usage; exit 0 ;;
      "")             usage; exit 2 ;;
    esac

    # 每个动作最多收几个参数，多给就报错。**静默扔掉参数**是这套 CLI 最坑人的错法
    # （踩过：`agent browser new_tab URL` 把 URL 吃了、开出一只空白标签，模型以为
    # 页面开了，对着白屏截了三张图 —— 见 docs/08-browser.md）。
    max=1
    case "$act_" in
      get_readable|back|list_tabs) max=0 ;;
      type|scroll)                 max=2 ;;
    esac
    [ $# -le "$max" ] || { echo "agent browser ${act_}：最多 $max 个参数，给了 $# 个" >&2; usage; exit 2; }

    case "$act_" in
      navigate)
        [ -n "${1:-}" ] || { usage; exit 2; }
        body="{\"action\":\"navigate\",\"url\":\"$(esc "$1")\"}" ;;
      get_backbone)
        if [ -n "${1:-}" ]; then int_arg "$1" "browser get_backbone"; body="{\"action\":\"get_backbone\",\"max_depth\":$1}"; else body='{"action":"get_backbone"}'; fi ;;
      click)
        [ -n "${1:-}" ] || { usage; exit 2; }
        body="{\"action\":\"click\",\"selector\":\"$(esc "$1")\"}" ;;
      type)
        [ -n "${1:-}" ] && [ -n "${2:-}" ] || { usage; exit 2; }
        body="{\"action\":\"type\",\"selector\":\"$(esc "$1")\",\"text\":\"$(esc "$2")\"}" ;;
      get_text)
        if [ -n "${1:-}" ]; then body="{\"action\":\"get_text\",\"selector\":\"$(esc "$1")\"}"; else body='{"action":"get_text"}'; fi ;;
      scroll)
        if [ -n "${2:-}" ]; then int_arg "$2" "browser scroll"; body="{\"action\":\"scroll\",\"direction\":\"${1:-down}\",\"amount\":$2}"; else body="{\"action\":\"scroll\",\"direction\":\"${1:-down}\"}"; fi ;;
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
      find_elements)
        [ -n "${1:-}" ] || { usage; exit 2; }
        body="{\"action\":\"find_elements\",\"selector\":\"$(esc "$1")\"}" ;;
      user_agent)
        case "${1:-}" in mobile|desktop) ;; *) echo "用法：agent browser user_agent mobile|desktop" >&2; exit 2 ;; esac
        body="{\"action\":\"set_user_agent\",\"user_agent\":\"$(esc "$1")\"}" ;;
      wait_for_dom_stable)
        if [ -n "${1:-}" ]; then int_arg "$1" "browser wait_for_dom_stable"; body="{\"action\":\"wait_for_dom_stable\",\"timeout\":$1}"; else body='{"action":"wait_for_dom_stable"}'; fi ;;
      js)
        [ -n "${1:-}" ] || { usage; exit 2; }
        body="{\"action\":\"js\",\"script\":\"$(esc "$1")\"}" ;;
      new_tab)
        if [ -n "${1:-}" ]; then body="{\"action\":\"new_tab\",\"url\":\"$(esc "$1")\"}"; else body='{"action":"new_tab"}'; fi ;;
      list_tabs) body='{"action":"list_tabs"}' ;;
      switch_tab)
        int_arg "${1:-}" "browser switch_tab"
        body="{\"action\":\"switch_tab\",\"tab\":$1}" ;;
      close_tab)
        if [ -n "${1:-}" ]; then int_arg "$1" "browser close_tab"; body="{\"action\":\"close_tab\",\"tab\":$1}"; else body='{"action":"close_tab"}'; fi ;;
      # 第一眼像 JSON 就原样发（复杂参数走这条）
      \{*) [ $# -eq 0 ] || { echo "JSON 形式后面不该再跟参数：$*" >&2; exit 2; }; body="$act_" ;;
      *) echo "agent browser：不认识的动作「${act_}」" >&2; usage; exit 2 ;;
    esac
    post /api/browser "$body""##;

const SEARCH_BODY: &str = r##"    shift || true
    if [ -z "${1:-}" ]; then
      echo "用法：agent search <检索意图> [条数]" >&2
      exit 2
    fi
    q=$(esc "$1")
    # format=text：让核心直接回排好版的文字（不是一串带 \n 转义的 JSON）
    if [ -n "${2:-}" ]; then
      int_arg "$2" search
      body="{\"query\":\"$q\",\"num_results\":$2,\"format\":\"text\"}"
    else
      body="{\"query\":\"$q\",\"format\":\"text\"}"
    fi
    post /api/websearch "$body""##;

const WEATHER_BODY: &str = r##"    shift || true
    post /api/weather "{\"argv\":$(json_args "$@")}""##;

const OPEN_BODY: &str = r##"    shift || true
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
      *'"ok":true'*) echo "已打开: $uri" ;;
      *)
        detail=$(printf '%s' "$out" | sed -n 's/.*"detail":"\([^"]*\)".*/\1/p')
        echo "没能打开（${detail:-没有响应}）：$uri" >&2
        ;;
    esac"##;

const KEY_BODY: &str = r##"    case "${2:-}" in
      back|home|recents|notifications) ;;
      *) echo "用法：agent key back|home|recents|notifications" >&2; exit 2 ;;
    esac
    act "{\"type\":\"key\",\"name\":\"$2\"}""##;

const TAP_BODY: &str = r##"    [ $# -eq 3 ] || { echo "用法：agent tap <x> <y>" >&2; exit 2; }
    int_arg "$2" tap; int_arg "$3" tap
    act "{\"type\":\"tap_xy\",\"x\":$2,\"y\":$3}""##;

const SWIPE_BODY: &str = r##"    [ $# -eq 5 ] || [ $# -eq 6 ] || { echo "用法：agent swipe <x1> <y1> <x2> <y2> [毫秒]" >&2; exit 2; }
    for _n in "$2" "$3" "$4" "$5" "${6:-300}"; do int_arg "$_n" swipe; done
    act "{\"type\":\"swipe\",\"x1\":$2,\"y1\":$3,\"x2\":$4,\"y2\":$5,\"ms\":${6:-300}}""##;

const LAUNCH_BODY: &str = r##"    [ $# -eq 2 ] || { echo "用法：agent launch <包名>（比如 com.android.settings）" >&2; exit 2; }
    act "{\"type\":\"launch\",\"package\":\"$(esc "$2")\"}""##;

const PHONE_BODY: &str = r##"    _name="$cmd"; shift || true
    # 截屏默认落到当前工作目录下面的 screenshots/（核心把它注在 ${AGENT_WORKDIR} 里）。
    # 显式给了 --path 就按给的来。
    if [ "$_name" = "screenshot" ] && [ $# -eq 0 ] && [ -n "${AGENT_WORKDIR:-}" ]; then
      set -- --path "${AGENT_WORKDIR}/screenshots/shot-$(date +%Y%m%d-%H%M%S).png"
    fi
    out=$(post /api/device "{\"cmd\":\"$_name\",\"argv\":$(json_args "$@")}")
    printf '%s\n' "$out"
    # 失败要变成退出码：模型常写 `agent sms list && …` 这种链，
    # 回了 0 的话它会拿着空输出继续往下跑。
    case "$out" in *'"ok":false'*) exit 1 ;; esac"##;

// 工具回的图片落在当前目录下（`$AGENT_WORKDIR`，核心那边见 `http::mcp_run`）。
// 出错时核心回的文字以「错误：」开头 —— 变成非 0 退出码，`&&` 链才会断。
const MCP_BODY: &str = r##"    shift || true
    out=$(post /api/mcp "{\"argv\":$(json_args "$@"),\"workdir\":\"$(esc "${AGENT_WORKDIR:-}")\"}")
    printf '%s\n' "$out"
    case "$out" in 错误：*) exit 1 ;; esac"##;

const FILES_BODY: &str = r##"    shift || true
    post /api/files "{\"path\":\"$(esc "${1:-/workspace}")\"}""##;

const SETUP_BODY: &str = r##"    shift || true
    exec /usr/local/bin/agent-setup "$@""##;

// 什么在跑：核心那边记着每条命令是哪个 proot，而 guest 进程的 TracerPid 就是它 ——
// 于是连刻意 setsid / nohup 脱离的常驻进程也能认出来（它们的父进程早就是 init 了，
// 靠 ps 自己认不出来）。`self` 参数把自己的进程组报上去，免得在「正在跑」里看到自己。
const PS_BODY: &str = r##"    self_pgid=$(awk '{print $5}' /proc/self/stat 2>/dev/null || echo 0)
    get "/api/procs?format=text&self=$self_pgid""##;

const KILL_BODY: &str = r##"    case "${2:-}" in
      ''|*[!0-9]*) echo "用法：agent kill <pid>（pid 从 agent ps 来）" >&2; exit 2 ;;
    esac
    pid="$2"
    # 先好声好气（让它收尾），几秒不走再来硬的
    kill -TERM "$pid" 2>/dev/null || true
    for _ in 1 2 3; do
      kill -0 "$pid" 2>/dev/null || { echo "已停止 $pid"; exit 0; }
      sleep 1
    done
    kill -KILL "$pid" 2>/dev/null || true
    if kill -0 "$pid" 2>/dev/null; then
      echo "停不掉 ${pid}（不是这个 uid 的进程？）" >&2
      exit 1
    fi
    echo "已强杀 $pid""##;

const DEBUG_BODY: &str = r##"    case "${2:-}" in
      state)    get /api/debug/state | fmt ;;
      request)  get /api/debug/request | fmt ;;
      tools)    get /api/debug/tools | tr ',' '\n' | sed -n 's/.*"name":"\([a-z_]*\)".*/\1/p' ;;
      sessions) get /api/sessions | fmt ;;
      session)  get /api/session | fmt ;;
      doctor)
        get /api/health | fmt
        echo '--- 跑一句 shell ---'
        post /api/linux '{"command":"echo ok"}'
        ;;
      get)  [ $# -eq 3 ] || { echo "用法：agent debug get /api/…" >&2; exit 2; }; get "$3" ;;
      post) [ $# -eq 4 ] || { echo "用法：agent debug post /api/… '{…}'" >&2; exit 2; }; post "$3" "$4" ;;
      *)
        echo "用法：agent debug state|request|tools|sessions|session|doctor" >&2
        echo "      agent debug get <path> / post <path> <json>" >&2
        exit 2
        ;;
    esac"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// 真跑一遍生成的脚本（`sh agent …`）：只看字符串里有没有不够，真正会错的是
    /// shell 本身（引号、heredoc、case 的写法）。
    fn run(m: Machine, args: &[&str]) -> (i32, String, String) {
        let dir = std::env::temp_dir().join(format!("agent-cli-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent");
        std::fs::write(&path, script(m)).unwrap();
        let out = std::process::Command::new("sh")
            .arg(&path)
            .args(args)
            .env("AGENT_PORT", "1")
            .output()
            .expect("跑不了 sh");
        let _ = std::fs::remove_dir_all(&dir);
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// 每台机器的 `agent help` 只列这台机器有的命令，而且**不提别的机器**。
    #[test]
    fn help_lists_only_what_this_machine_has() {
        for m in Machine::ALL {
            let (code, help, err) = run(m, &["help"]);
            assert_eq!(code, 0, "{m:?} 的 agent help 失败：{err}");
            for s in ["agent browser", "agent search", "agent weather", "agent open", "agent mcp", "agent debug"] {
                assert!(help.contains(s), "{m:?} 的 help 里少了 {s}");
            }
            // 模型不该看到「手机上 / 电脑上 / 只有…才有」这层区分
            for leak in ["手机上", "电脑上", "只有手机", "两端", "宿主模式"] {
                assert!(!help.contains(leak), "{m:?} 的 help 里提到了「{leak}」：\n{help}");
            }
            let phone = m == Machine::Phone;
            for c in crate::phone::COMMANDS {
                assert_eq!(
                    help.contains(&format!("agent {}", c.name)),
                    phone,
                    "{m:?}：agent {} 该不该列",
                    c.name
                );
            }
            for s in ["agent key", "agent tap", "agent swipe", "agent launch"] {
                assert_eq!(help.contains(s), phone, "{m:?}：{s} 该不该列");
            }
            let proot = m != Machine::Host;
            for s in ["agent ps", "agent kill", "agent setup", "agent files"] {
                assert_eq!(help.contains(s), proot, "{m:?}：{s} 该不该列");
            }
        }
    }

    /// 没有的命令就是不认识 —— 不是「这台机器上没有」那种解释。
    #[test]
    fn missing_commands_are_simply_unknown() {
        for (m, cmd) in [
            (Machine::Host, "sms"),
            (Machine::Host, "tap"),
            (Machine::Host, "ps"),
            (Machine::Sandbox, "clipboard"),
            (Machine::Sandbox, "key"),
        ] {
            let (code, _, err) = run(m, &[cmd]);
            assert_eq!(code, 2, "{m:?} {cmd}");
            assert!(err.contains("不认识的子命令"), "{m:?} {cmd}：{err}");
        }
    }

    /// 参数校验在脚本里就拦下来（不用连核心）。
    #[test]
    fn device_actions_validate_their_arguments() {
        let bad: &[&[&str]] = &[
            &["key", "power"],
            &["tap", "1"],
            &["tap", "x", "2"],
            &["swipe", "1", "2", "3"],
            &["launch"],
            &["kill", "abc"],
            &["debug"],
        ];
        for args in bad {
            let (code, _, err) = run(Machine::Phone, args);
            assert_eq!(code, 2, "{args:?} 该报用法错：{err}");
            assert!(err.contains("agent"), "{args:?}：{err}");
        }
    }

    /// 浏览器动作表和核心（`cdp::ACTIONS`）对得上：漂了就是模型发得出、核心不认，
    /// 或者核心有、CLI 没入口（模型永远发现不了）。
    #[test]
    fn browser_actions_match_the_core() {
        let s = script(Machine::Host);
        for action in crate::cdp::ACTIONS {
            let name = if *action == "set_user_agent" { "user_agent" } else { action };
            assert!(s.contains(&format!("      {name})")), "CLI 里没有浏览器动作 {name}");
        }
    }

    /// 系统能力那一个 case 分支，名字和 `phone::COMMANDS` 一字不差。
    #[test]
    fn phone_commands_come_from_the_phone_table() {
        let s = script(Machine::Phone);
        let want = format!("  {})\n", crate::phone::names().join("|"));
        assert!(s.contains(&want), "系统能力那个分支不对，应该是 {want}");
    }

    /// 提示词里那份清单：每台机器只列自己有的，不提别的机器。
    #[test]
    fn prompt_clause_follows_the_machine() {
        let phone = prompt_clause(Machine::Phone);
        assert!(phone.contains("`agent key|tap|swipe|launch`"), "{phone}");
        assert!(phone.contains(&format!("`agent {}`", crate::phone::names().join("|"))), "{phone}");
        assert!(phone.contains("真 WebView") && phone.contains("`agent ps`"), "{phone}");

        let host = prompt_clause(Machine::Host);
        assert!(host.contains("无头 Chrome") && host.contains("默认程序"), "{host}");
        for s in ["agent ps", "agent key", "agent sms", "agent debug"] {
            assert!(!host.contains(s), "宿主上不该提 {s}：{host}");
        }
        let sandbox = prompt_clause(Machine::Sandbox);
        assert!(sandbox.contains("`agent ps`") && !sandbox.contains("agent key"), "{sandbox}");
    }

    /// 浏览器的动作清单 help 和提示词里都有，而且是同一份。
    #[test]
    fn browser_actions_are_in_help_and_prompt() {
        for m in Machine::ALL {
            let (_, help, _) = run(m, &["help"]);
            let prompt = prompt_clause(m);
            for a in BROWSER_ACTIONS {
                assert!(help.contains(a), "{m:?} 的 help 里少了 {a}");
                assert!(prompt.contains(a), "{m:?} 的提示词里少了 {a}");
            }
            assert!(prompt.contains("`agent search <检索意图> [条数]`"), "{prompt}");
        }
    }

    /// `$var` 后面**不能**紧跟中文标点，必须写 `${var}`（见模块说明）。
    #[test]
    fn variables_are_braced_before_multibyte() {
        for m in Machine::ALL {
            let s = script(m);
            let b = s.as_bytes();
            let name = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
            let mut bad: Vec<String> = Vec::new();
            let mut i = 0usize;
            while i < b.len() {
                if b[i] == b'$' && i + 1 < b.len() && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'_') {
                    let mut j = i + 1;
                    while j < b.len() && name(b[j]) {
                        j += 1;
                    }
                    if j < b.len() && b[j] >= 0x80 {
                        bad.push(String::from_utf8_lossy(&b[i..j]).into_owned());
                    }
                }
                i += 1;
            }
            assert!(bad.is_empty(), "{m:?}：这些变量后面紧跟多字节字符：{bad:?}");
        }
    }
}
