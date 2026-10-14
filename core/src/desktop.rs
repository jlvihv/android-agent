//! 桌面（宿主模式）：**这台电脑本身**就是设备。
//!
//! 手机上设备通道背后是无障碍服务（点 / 滑 / 按键 / 起 App / 开 URI）。电脑上
//! 不能拿 mock 顶：它对什么都回「成功」（`agent open` 说「已让手机打开」其实什么
//! 都没开，点击回一句假的 `mock: tapped`）。这台是真的：
//!
//! - `open`：交给系统的打开器（macOS `open`，Linux `xdg-open`）—— 网页、文件、
//!   `mailto:` 都由用户自己的默认程序接；
//! - `launch`：macOS 上 `open -a <应用名>`；Linux 没有统一的「按名字起应用」，
//!   明说让模型直接在 bash 里跑它的命令；
//! - 点 / 滑 / 按键：明说「电脑上没有」，不假装做了。
//!
//! mock 还留着，给 `just dev`（在电脑上复现手机）和测试用。

use std::path::PathBuf;
use std::process::{Command as Proc, Stdio};
use std::time::Duration;

use crate::state::{Command, DeviceApi};

/// 桌面上我们自己的东西放哪（`agent` 命令、状态文件、截图、Chrome profile、库）。
///
/// `$XDG_DATA_HOME/android-agent`，默认 `~/.local/share/android-agent` ——
/// macOS 上也是这个，**故意不用** `~/Library/Application Support`：截图路径会原样
/// 出现在 `read` / `![](sandbox:…)` 里，带空格的路径会让 Markdown 链接断掉。
/// 也故意不放进 `~/.local/bin` 那一类用户目录：这是我们自己的东西，不往用户的
/// PATH 配置里塞。
pub fn data_root() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/share"))
        })
        .unwrap_or_else(|| std::env::temp_dir().join("android-agent"));
    base.join("android-agent")
}

// ---------------------------------------------------------------------------
// 局域网：广播自己 + 配对（手机 App 的「选择机器」用）
// ---------------------------------------------------------------------------

/// mDNS 服务类型。手机那边（`MainActivity` 的 NsdManager）找的就是它。
pub const SERVICE: &str = "_agent-core._tcp";

/// 输错几次就不再接受配对（直到核心重启）。6 位数字 = 一百万种，5 次猜中的概率
/// 是二十万分之一。
const PAIR_MAX_FAILS: u32 = 5;

/// 一次性的配对码：手机第一次连这台电脑时输入它，换回访问凭据。
///
/// 为什么要这一步：自动发现只给得出「地址」，给不出 token —— 把 token 放进
/// mDNS 的 TXT 记录等于广播给局域网里的所有人。参照安卓「无线调试」的配对：
/// 电脑上显示一个码，人在手机上输一次，之后手机记住 token，再也不用输。
pub struct Pairing {
    code: String,
    fails: std::sync::atomic::AtomicU32,
}

impl Default for Pairing {
    fn default() -> Self {
        Self::new()
    }
}

impl Pairing {
    pub fn new() -> Self {
        let n = u32::from_le_bytes(
            uuid::Uuid::new_v4().as_bytes()[..4]
                .try_into()
                .unwrap_or([0; 4]),
        );
        Self {
            code: format!("{:06}", n % 1_000_000),
            fails: Default::default(),
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    /// 还能输错几次；`0` = 已经停用（重启核心换新码）。设置页要显示这个状态。
    pub fn remaining(&self) -> u32 {
        PAIR_MAX_FAILS.saturating_sub(self.fails.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// 核对配对码。输错 `PAIR_MAX_FAILS` 次之后一律拒绝（防猜）。
    pub fn check(&self, code: &str) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        if self.fails.load(Ordering::Relaxed) >= PAIR_MAX_FAILS {
            return Err("配对码输错太多次，已停用 —— 重启电脑上的核心会换一个新码".into());
        }
        let code = code.trim();
        // 耗时和内容无关的比较（别让快慢泄露前几位）
        let same = code.len() == self.code.len()
            && code
                .bytes()
                .zip(self.code.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;
        if same {
            return Ok(());
        }
        let n = self.fails.fetch_add(1, Ordering::Relaxed) + 1;
        Err(format!(
            "配对码不对（还能试 {} 次）",
            PAIR_MAX_FAILS.saturating_sub(n)
        ))
    }
}

/// 这台电脑给人看的名字（macOS 的「电脑名称」；Linux 用主机名）。
///
/// 只取一次：macOS 上要起一个 `scutil` 子进程，而 `/api/health` 每次都要它
/// （那是 async handler，同步等子进程会卡住一个 tokio worker）。
pub fn computer_name() -> String {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(probe_computer_name).clone()
}

fn probe_computer_name() -> String {
    if cfg!(target_os = "macos") {
        if let Ok(o) = Proc::new("scutil").args(["--get", "ComputerName"]).output() {
            let n = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if o.status.success() && !n.is_empty() {
                return n;
            }
        }
    }
    let mut buf = [0u8; 256];
    // SAFETY: gethostname 最多写 buf.len() 个字节
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let n = String::from_utf8_lossy(&buf[..end]).trim().to_string();
        if !n.is_empty() {
            return n;
        }
    }
    "电脑".into()
}

/// 广播用哪个名字：默认是这台电脑的名字（`computer_name`），`AGENT_ADVERT_NAME` 可以换。
///
/// 为什么需要这个开关：同一台机器上跑**两个**核心时（比如在另一个端口上验新构建），
/// 两个同名服务在 mDNS 里没法共存 —— 先起的那个报「本地名称冲突」静默死掉，
/// 手机上只剩下后起的那个。给第二个实例换个名字：
///
/// ```sh
/// AGENT_ADVERT_NAME="jl 测试" AGENT_PORT=8789 ./core/target/release/agent-dev
/// ```
pub fn advert_name() -> String {
    std::env::var("AGENT_ADVERT_NAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(computer_name)
}

/// 广播进程起来之后，隔多久去确认它还活着（见 `Advert::failure`）。
///
/// `avahi-publish-service` 跟 daemon 说上话要几十毫秒；这个值取得比它宽裕，
/// 又短到人感觉不到（启动时只多这么一下）。
pub const ADVERT_SETTLE: Duration = Duration::from_millis(300);

/// 正在广播的 mDNS 服务。`Drop` 就停（核心退出时不留一条指向死端口的广播）。
///
/// **只看 `spawn` 成不成功是不够的**：`avahi-publish-service` 起来之后立刻退出是
/// 常态，而且退得很安静 —— 只往 stderr 说一句，退出码还是 0：
///
/// - `avahi-daemon` 没跑：`创建客户端对象失败：Daemon 不在运行`（这台开发机上
///   默认就是没跑，于是「手机上点「jl」」一直是句空头支票）；
/// - 这个名字已经有另一个核心在广播：`添加服务失败：本地名称冲突`（同名服务在
///   mDNS 里没法共存，后起的赢，先起的悄悄死掉）。
///
/// 所以 stderr 收进管道不丢，`failure()` 隔 `ADVERT_SETTLE` 去问一句：还活着 =
/// 真在广播；退了 = 把 stderr 那句原因原样带回去，`main` 打给人和日志看。
pub struct Advert {
    child: std::process::Child,
    /// 已经问出来的原因（问过就不再问）；还活着的时候是 `None`
    error: Option<String>,
}

impl Advert {
    /// 还在广播吗？不在了就把原因带回来（stderr 第一行非空内容）。
    ///
    /// 调用点要在 `ADVERT_SETTLE` 之后：刚 spawn 出来的那几十毫秒里它还没把注册
    /// 请求发给 daemon，这时候问必然「还活着」。
    pub fn failure(&mut self) -> Option<String> {
        if let Some(e) = &self.error {
            return Some(e.clone());
        }
        // 还活着 = 在广播（`?`：`try_wait` 还没退就是 None）。
        // 收尸失败（`.ok()` 摊平掉）也当还活着 —— 报错的该是「它在退」，不是这里。
        self.child.try_wait().ok().flatten()?;
        let mut raw = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut pipe, &mut raw);
        }
        // 它那一句话（daemon 不在、名字冲突）就是全部线索，只取第一行非空的
        let why = raw
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_string)
            // 没话说也得说一句：静默死掉正是这个函数要消灭的东西
            .unwrap_or_else(|| "广播进程退出了，但没说什么".into());
        self.error = Some(why.clone());
        Some(why)
    }
}

impl Drop for Advert {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 在局域网里广播「这里有个核心」（mDNS / DNS-SD）。
///
/// 用系统自带的工具，不引 Rust 的 mDNS 库：macOS 是 `dns-sd`（mDNSResponder），
/// Linux 是 `avahi-publish-service`。除了零依赖，更要紧的是**它们按网卡回答地址**
/// —— 自己实现的话，开着 Clash TUN 的机器会把 198.18.x.x 那张虚拟网卡的地址也
/// 广播出去，手机解析到它就连不上（打印地址时也是同一个坑）。
///
/// TXT 里只有实例 id 和版本 —— **没有凭据**（见 `Pairing`）。
/// 工具不在就返回 None（那手机 App 就找不到这台，只有浏览器能直接开地址）；工具在、但起不来（daemon
/// 没跑、名字撞了）是另一回事，由 `Advert::failure` 负责说出来。
pub fn advertise(name: &str, port: u16, id: &str) -> Option<Advert> {
    let port = port.to_string();
    let txt = [format!("id={id}"), format!("v={}", crate::VERSION)];
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Proc::new("dns-sd");
        c.args(["-R", name, SERVICE, "local", &port]);
        c
    } else {
        let mut c = Proc::new("avahi-publish-service");
        c.args([name, SERVICE, &port]);
        c
    };
    cmd.args(&txt);
    start(&mut cmd)
}

/// 起广播进程。单独一层是为了能测「起不来时能不能把原因说清楚」—— 测试里塞一个
/// 立刻退出的假工具就行，不依赖这台机器上真的装了 avahi / dns-sd（见 tests）。
fn start(cmd: &mut Proc) -> Option<Advert> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        // **stderr 不丢**：它那一句是唯一的失败线索（见 `Advert`）
        .stderr(Stdio::piped());
    match cmd.spawn() {
        Ok(child) => Some(Advert { child, error: None }),
        Err(e) => {
            tracing::warn!(
                "没法在局域网里广播（{:?}: {e}）—— 手机 App 里搜不到这台电脑",
                cmd.get_program()
            );
            None
        }
    }
}

/// 这台电脑当设备用。
pub struct HostDevice;

impl DeviceApi for HostDevice {
    fn name(&self) -> String {
        "desktop".into()
    }

    fn execute(&self, cmd_json: &str) -> Result<String, String> {
        let cmd: Command = serde_json::from_str(cmd_json).map_err(|e| e.to_string())?;
        match cmd {
            Command::Open { url } => {
                let opener = if cfg!(target_os = "macos") {
                    "open"
                } else {
                    "xdg-open"
                };
                run(Proc::new(opener).arg(&url))?;
                Ok(format!("已用这台电脑的默认程序打开 {url}"))
            }
            Command::Launch { package } => {
                if cfg!(target_os = "macos") {
                    run(Proc::new("open").args(["-a", &package]))?;
                    Ok(format!("已启动 {package}"))
                } else {
                    Err(format!(
                        "Linux 上没有统一的「按名字起应用」：直接在 bash 里跑它的命令\
                         （比如 `setsid {package} >/dev/null 2>&1 &`）"
                    ))
                }
            }
            Command::TapXy { .. } | Command::Swipe { .. } | Command::Key { .. } => Err(
                "电脑上没有点击 / 滑动 / 按键 —— 那是手机上的设备操作。网页用 agent browser".into(),
            ),
        }
    }
}

/// 跑一下，非零退出就把 stderr 当错误带回去（「没有这个应用」之类要让模型看到）。
fn run(cmd: &mut Proc) -> Result<(), String> {
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("起不来 {:?}: {e}", cmd.get_program()))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if err.is_empty() {
        format!("{:?} 失败（{}）", cmd.get_program(), out.status)
    } else {
        err
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_codes_are_six_digits_and_lock_after_five_misses() {
        let p = Pairing::new();
        assert_eq!(p.code().len(), 6);
        assert!(p.code().bytes().all(|b| b.is_ascii_digit()));
        assert!(
            p.check(&format!(" {} ", p.code())).is_ok(),
            "首尾空格不算错"
        );
        let wrong = if p.code() == "000000" {
            "111111"
        } else {
            "000000"
        };
        assert_eq!(p.remaining(), PAIR_MAX_FAILS, "一次没错时是满的");
        for i in 0..PAIR_MAX_FAILS {
            assert!(p.check(wrong).is_err());
            assert_eq!(p.remaining(), PAIR_MAX_FAILS - i - 1, "错一次少一次");
        }
        assert!(
            p.check(p.code()).unwrap_err().contains("停用"),
            "输错太多次后对的也不行"
        );
        assert_eq!(p.remaining(), 0, "停用之后还是 0（设置页靠它显示状态）");
    }

    /// 广播进程得会「说实话」：起不来（daemon 没跑、名字撞了）时，stderr 里那句
    /// 原因要能原样带回给 main —— 不然终端会照着「手机上点它」往下打，而手机上
    /// 什么都搜不到（这台机器上就这么空转了很久：avahi-daemon 根本没装）。
    #[test]
    fn a_dead_broadcaster_reports_why() {
        let mut c = Proc::new("sh");
        c.args(["-c", "echo '添加服务失败：本地名称冲突' >&2; exit 0"]);
        let mut a = start(&mut c).expect("spawn 不该失败");
        // 真机上这里是 main 等 ADVERT_SETTLE 之后再问；测试里直接等它收尸
        let _ = a.child.wait();
        assert_eq!(a.failure().as_deref(), Some("添加服务失败：本地名称冲突"));
        // 问过一次就不再问（留着那份原因）
        assert_eq!(a.failure().as_deref(), Some("添加服务失败：本地名称冲突"));
    }

    /// 还在跑的不能被当成失败 —— 判定看的是「退没退」，不是「stderr 里有没有东西」
    /// （avahi 真在广播时也可能往 stderr 说话）。
    #[test]
    fn a_live_broadcaster_is_not_a_failure() {
        let mut c = Proc::new("sh");
        c.args(["-c", "echo 噪声 >&2; sleep 30"]);
        let mut a = start(&mut c).expect("spawn 不该失败");
        assert!(a.failure().is_none(), "还活着就是没问题");
        drop(a); // Drop 会把它收掉
    }

    /// 「两个核心同时跑」那条路的开关：给了 `AGENT_ADVERT_NAME` 就用它，
    /// 全是空白当没给（不然会广播出一个空名字，手机上是个空行）。
    #[test]
    fn advert_name_can_be_overridden() {
        let before = std::env::var("AGENT_ADVERT_NAME").ok();
        std::env::set_var("AGENT_ADVERT_NAME", "  jl 测试  ");
        assert_eq!(advert_name(), "jl 测试", "两头空格去掉");
        std::env::set_var("AGENT_ADVERT_NAME", "   ");
        assert_eq!(advert_name(), computer_name(), "全空白 = 没给");
        match before {
            Some(v) => std::env::set_var("AGENT_ADVERT_NAME", v),
            None => std::env::remove_var("AGENT_ADVERT_NAME"),
        }
    }

    #[test]
    fn it_refuses_phone_gestures_instead_of_pretending() {
        let dev = HostDevice;
        for cmd in [
            r#"{"type":"tap_xy","x":1,"y":2}"#,
            r#"{"type":"key","name":"back"}"#,
            r#"{"type":"swipe","x1":0,"y1":0,"x2":1,"y2":1}"#,
        ] {
            let err = dev.execute(cmd).unwrap_err();
            assert!(err.contains("电脑上没有"), "{cmd}: {err}");
        }
        assert!(dev.execute("不是 JSON").is_err());
    }
}
