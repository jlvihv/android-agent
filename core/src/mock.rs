//! 桌面开发用的假设备（mock）。
//!
//! 存在意义：让「Rust 核心 + Web 前端」这条链路在**没有安卓设备**的情况下
//! 也能跑通（决策 D3 的最大好处 —— 同源 localhost 架构在桌面浏览器里
//! 就是一个普通网页）。接上真机后换成 Kotlin 的 `DeviceApi` 实现即可。
//!
//! 它**不假装**能读懂界面：读界面 / 按节点索引操作那两个工具（连同一套假 UI 树、
//! 回推的快照）已经删掉，能力收进了沙箱里的一个命令，所以这里只剩「能收指令、
//! 能回执」这一层，用来验证 `DeviceApi` 这条通路是通的。

use std::sync::{Arc, Mutex};

use crate::state::{Command, DeviceApi};

#[derive(Default)]
struct MockState {
    /// 收到过多少条指令 —— 回执里带上，好看出通路真的走通了
    commands: u32,
}

#[derive(Default)]
pub struct MockDevice {
    inner: Mutex<MockState>,
}

impl MockDevice {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

impl DeviceApi for MockDevice {
    fn name(&self) -> String {
        "mock-device".into()
    }

    fn execute(&self, cmd_json: &str) -> Result<String, String> {
        // 解析一遍：非法指令照样报错，这条通路的意义就在这
        let cmd: Command = serde_json::from_str(cmd_json).map_err(|e| e.to_string())?;
        let mut s = self.inner.lock().map_err(|_| "mock poisoned".to_string())?;
        s.commands += 1;
        Ok(match cmd {
            Command::TapXy { x, y } => format!("mock: tapped ({x},{y}) #{}", s.commands),
            Command::Swipe { .. } => format!("mock: swiped #{}", s.commands),
            Command::Key { name } => format!("mock: pressed {name} #{}", s.commands),
            Command::Launch { package } => format!("mock: launched {package} #{}", s.commands),
            Command::Open { url } => format!("mock: opened {url} #{}", s.commands),
        })
    }
}
