//! 桌面（宿主模式）：**这台电脑本身**就是设备。
//!
//! 手机上设备通道背后是无障碍服务（点 / 滑 / 按键 / 起 App / 开 URI）。电脑上
//! 以前塞的是 mock —— 它对什么都回「成功」：`agent open` 说「已让手机打开」其实
//! 什么都没开，`agent command` 的点击也回一句假的 `mock: tapped`。现在：
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
                "电脑上没有点击 / 滑动 / 按键 —— 那是手机上的设备操作。网页用 agent browser"
                    .into(),
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
