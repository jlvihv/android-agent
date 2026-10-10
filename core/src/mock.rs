//! 桌面开发用的假设备（mock）。
//!
//! 存在意义：让「Rust 核心 + Web 前端」这条链路在**没有安卓设备**的情况下
//! 就能跑通和验证（决策 D3 的最大好处 —— 同源 localhost 架构在桌面浏览器里
//! 就是一个普通网页）。接上真机后换成 Kotlin 的 `DeviceApi` 实现即可。

use std::sync::{Arc, Mutex, Weak};

use crate::state::{AppState, Command, DeviceApi, Flags, Node, Screen, UiSnapshot};

#[derive(Default)]
struct MockState {
    /// 0=Home 1=Settings 2=Browser
    screen: u8,
    toggle_on: bool,
    typed: String,
    taps: u32,
}

pub struct MockDevice {
    state: Weak<AppState>,
    inner: Mutex<MockState>,
}

impl MockDevice {
    pub fn new(state: Weak<AppState>) -> Arc<Self> {
        Arc::new(Self {
            state,
            inner: Mutex::new(MockState::default()),
        })
    }

    fn push_snapshot(&self) {
        let Some(st) = self.state.upgrade() else { return };
        let snap = self.snapshot();
        if let Ok(mut g) = st.snapshot.write() {
            *g = Some(snap.clone());
        }
        st.emit(crate::state::Event::Snapshot { data: snap });
    }

    fn snapshot(&self) -> UiSnapshot {
        let s = self.inner.lock().unwrap();
        let (package, activity, mut nodes) = match s.screen {
            1 => (
                "com.android.settings",
                "SettingsActivity",
                vec![
                    node(0, -1, "LinearLayout", "", "", [0, 0, 1080, 2400], Flags::default(), ""),
                    node(1, 0, "TextView", "设置", "settings/title", [0, 120, 1080, 220], Flags::default(), ""),
                    node(
                        2,
                        0,
                        "Switch",
                        "Wi-Fi",
                        "settings/wifi",
                        [80, 300, 1000, 420],
                        Flags { clickable: true, checkable: true, focusable: true, enabled: true, ..Default::default() },
                        if s.toggle_on { "checked=true" } else { "checked=false" },
                    ),
                    node(
                        3,
                        0,
                        "Button",
                        "返回",
                        "settings/back",
                        [80, 500, 1000, 620],
                        Flags { clickable: true, focusable: true, enabled: true, ..Default::default() },
                        "",
                    ),
                ],
            ),
            2 => (
                "com.android.chrome",
                "BrowserActivity",
                vec![
                    node(0, -1, "LinearLayout", "", "", [0, 0, 1080, 2400], Flags::default(), ""),
                    node(
                        1,
                        0,
                        "EditText",
                        &s.typed,
                        "browser/url",
                        [80, 120, 1000, 240],
                        Flags { clickable: true, editable: true, focusable: true, enabled: true, ..Default::default() },
                        "password=false",
                    ),
                    node(
                        2,
                        0,
                        "Button",
                        "前往",
                        "browser/go",
                        [80, 300, 1000, 420],
                        Flags { clickable: true, focusable: true, enabled: true, ..Default::default() },
                        "",
                    ),
                    node(
                        3,
                        0,
                        "Button",
                        "返回",
                        "browser/back",
                        [80, 500, 1000, 620],
                        Flags { clickable: true, focusable: true, enabled: true, ..Default::default() },
                        "",
                    ),
                ],
            ),
            _ => (
                "com.example.launcher",
                "HomeActivity",
                vec![
                    node(0, -1, "LinearLayout", "", "", [0, 0, 1080, 2400], Flags::default(), ""),
                    node(1, 0, "TextView", "Mock 桌面", "home/title", [0, 120, 1080, 220], Flags::default(), ""),
                    node(
                        2,
                        0,
                        "Button",
                        "打开设置",
                        "home/settings",
                        [80, 300, 1000, 420],
                        Flags { clickable: true, focusable: true, enabled: true, ..Default::default() },
                        "",
                    ),
                    node(
                        3,
                        0,
                        "Button",
                        "打开浏览器",
                        "home/browser",
                        [80, 500, 1000, 620],
                        Flags { clickable: true, focusable: true, enabled: true, ..Default::default() },
                        "",
                    ),
                    node(
                        4,
                        0,
                        "Button",
                        "刷新快照",
                        "home/refresh",
                        [80, 700, 1000, 820],
                        Flags { clickable: true, focusable: true, enabled: true, ..Default::default() },
                        "",
                    ),
                ],
            ),
        };
        let _ = &mut nodes;
        UiSnapshot {
            ts: now_ms(),
            package: package.into(),
            activity: activity.into(),
            screen: Screen { w: 1080, h: 2400 },
            nodes,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn node(
    i: u32,
    p: i32,
    class: &str,
    text: &str,
    id: &str,
    b: [i32; 4],
    a: Flags,
    s: &str,
) -> Node {
    Node {
        i,
        p,
        c: class.into(),
        t: text.into(),
        id: id.into(),
        b,
        a,
        s: s.into(),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl DeviceApi for MockDevice {
    fn name(&self) -> String {
        "mock-device".into()
    }

    fn execute(&self, cmd_json: &str) -> Result<String, String> {
        let cmd: Command = serde_json::from_str(cmd_json).map_err(|e| e.to_string())?;
        // 读操作也要回推快照，否则前端拿不到任何东西
        let force = matches!(&cmd, Command::Snapshot);
        let mut changed = false;

        {
            let mut s = self.inner.lock().map_err(|_| "mock poisoned".to_string())?;
            match cmd {
                Command::Snapshot => {}
                Command::TapNode { i } => {
                    s.taps += 1;
                    changed = true;
                    match (s.screen, i) {
                        (0, 2) => s.screen = 1,
                        (0, 3) => s.screen = 2,
                        (0, 4) => {}
                        (1, 2) => s.toggle_on = !s.toggle_on,
                        (1, 3) => s.screen = 0,
                        (2, 2) => s.typed = format!("https://example.com/{}", s.taps),
                        (2, 3) => s.screen = 0,
                        _ => {}
                    }
                }
                Command::Key { ref name } if name == "back" => {
                    s.screen = 0;
                    changed = true;
                }
                Command::SetText { text, .. } => {
                    s.typed = text;
                    changed = true;
                }
                Command::Launch { .. } | Command::TapXy { .. } | Command::Swipe { .. } => {
                    changed = true;
                }
                _ => {}
            }
        }

        if changed || force {
            self.push_snapshot();
        }
        Ok(format!("mock ok (changed={changed})"))
    }
}
