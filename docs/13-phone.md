# 手机系统能力：`agent device|clipboard|volume|brightness|torch|vibrate|screenshot|radios|alarm|location|contacts|calendar|sms|notify|speak`

> 2026-10-15 加了这一批。手机上跑过（模拟器真机行为），电脑上**列都不列**。

## 1. 一句话

模型在沙箱里敲 `agent sms list`，核心把它转给 Kotlin 的 `DeviceApisImpl`，
后者用 Android API 干活、回一段 JSON。**它们只有手机上有** —— 电脑上的
`agent help` 里没有这一段，敲了也直接拒绝。

## 2. 为什么不做成工具

同 [docs/10](10-tools-vs-sandbox.md)：能在 shell 里说清的能力不做成工具。
设备命令尤其明显 —— 「查联系人然后给第一个人发短信」做成工具是两轮 LLM 请求，
写成一条 bash 是一轮：

```sh
id=$(agent contacts search 张三 -q | jq -r '.data.contacts[0].id')
tel=$(agent contacts get "$id" -q | jq -r '.data.phones[0]')
echo "要发给 $tel"
```

## 3. 按机器显示不同的子命令（这次的重点）

`agent` 脚本是**按机器生成**的（`core/src/cli.rs`）：手机上那份有这一段，电脑上那份
**根本没有这些子命令** —— 不是运行时判断后拒绝，模型在电脑上压根看不到它们，也就看不到
「这台机器上没有」这层区分。

```sh
$ agent help          # 手机上
  …
  agent device [info|battery|storage]     设备信息（不带子命令 = info）
  agent clipboard get | set --text T | clear | status
  agent volume [get|set|up|down|max|mute|unmute] [--stream music|ring|…]
  …
```

help、`case` 分发、系统提示词里那份清单都从同一张表（`cli::subcommands()`，系统能力
那一项的名字和用法从 `phone::COMMANDS` 来）按机器取，不会一处有、一处没有。

**判据是编译目标（`cfg!(target_os = "android")`），不是 `Platform`。** 桌面上的
`just dev`（proot 沙箱）也是 `Platform::Android` —— 形态和手机一模一样，但它没有
短信、没有闹钟。写错了的表现是模型拿一条根本跑不通的命令反复试。

后端还有第四道兜底：桌面上 `state.phone()` 一直是 `None`，`/api/device` 会回
`{"ok":false,"error":{"code":"unavailable",…}}`。

## 4. 三个门，各有各的开法

| 门 | 接口 | 要什么 | 什么时候注入 |
|---|---|---|---|
| `device`（点按/滑动/按键） | `DeviceApi` | 无障碍服务 | 服务连上时 |
| `browser`（WebView） | `BrowserApi` | 无 | `AgentApp.onCreate` |
| **`phone`（这批）** | **`PhoneApi`** | **普通运行时权限** | **`AgentApp.onCreate`** |

三件事的可用条件完全不同，所以是三个 trait、三条 FFI 通道，不是一个「设备」大接口。
（照 `BrowserApi` 的先例，见 `state.rs` 里的说明。）

## 5. 权限：把「弹窗 + 等人点」包成一次阻塞调用

设备命令跑在 Rust 的 `spawn_blocking` 线程上，它要的是一个答案；而权限弹窗只能从
Activity 发、结果只能从 `onRequestPermissionsResult` 回。`AgentPermissions` 用一把
闩把两头接上（`ensure()` 阻塞等待）。

三条规矩，都有代价在后面撑着：

1. **只在人真的在的时候弹**（`MainActivity.current != null`）。后台弹权限框在
   Android 10+ 根本不显示，用户回来只看到一句「超时」。
2. **问过就记一笔**（SharedPreferences）。系统"不再询问"之后，再请求不会出现任何
   窗口 —— 那就别假装在等：下次直接回「没拿到」，让模型去说「去设置里手动开」。
3. **超时 25 秒**。人要真在看着屏幕，几秒就点了；25 秒没人点就是人不在。
   一个回合里白等 90 秒比"再来一次"糟得多。

`MainActivity` 里那个 `current`（onPause 清）**不能**用来起系统界面：
`agent alarm set` 会把时钟 App 拉到前台、我们只是 paused，紧接着的
`agent alarm timer` 就不该失败。所以另有 `alive`（onCreate 起、onDestroy 清）
专门给 `startActivity` 用。

## 6. 一条命令长什么样

```sh
$ agent sms list --limit 2
{"ok":true,"data":{"messages":[{"id":"1","address":"10086","body":"验证码 123456","date":"2026-10-15T20:19:19","type":"inbox"}]}}

$ agent location current
{"ok":true,"data":{"latitude":31.2304,"longitude":121.4737,"accuracy_m":5,"source":"fresh", …}}
```

- 输出**始终是那一行 JSON**（`{"ok":…,"data":…}` 或 `{"ok":false,"error":{"code","message"}}`）。
  `ok:false` 时 CLI 以非 0 退出 —— `&&` 链因此会断，模型不会拿着空输出往下走。
- `--help` 每个命令都认（用法文本由 Kotlin 回，避免两处各写一份）。
- 参数从 shell 原样传过去（`{"cmd":"sms","argv":["list","--limit","2"]}`），
  解析在 Kotlin（`PhoneArgs`）：Rust 那边不需要懂每个命令的参数，加子命令不用动它。

## 7. 加一个新能力要动哪几处

1. `core/src/phone.rs` 的 `COMMANDS` 加一项（名字 + `agent help` 里的用法）——
   这一份是唯一的清单，脚本的分发、help、提示词都从它生成；
2. `DeviceApisImpl.kt` 加一个分支。

有测试盯着（`cli.rs`）：

- `phone_commands_come_from_the_phone_table`：脚本里那个 case 分支和 `phone::COMMANDS` 一字不差；
- `help_lists_only_what_this_machine_has`：真跑每台机器的 `sh agent help`，
  手机上全有、电脑上一条都没有，也不许出现「手机上 / 电脑上」这种字眼；
- `prompt::device_commands_are_only_promised_where_they_exist`：提示词同规则。

## 8. 验证记录（2026-10-15）

模拟器（sdk_gphone64_x86_64 / Android 15）+ 桌面 host 模式，都跑过：

| 命令 | 结果 |
|---|---|
| `device info`（默认）/ `--help` | ✅ 型号 / 电池 / 存储，用法文本；真机 Pixel 10 / Android 17 |
| `clipboard set/get/status` | ✅ 写进去读得回来 |
| `volume get/set --percent/down` | ✅ 真机上 7/25 → 8/25 → 7/25（带百分比），测完恢复原值；超量程与错 stream 都报错 |
| `torch on/off` | ✅ 真机上手电筒真的开关 |
| `vibrate --pattern tap` | ✅ 真机上振了 50ms |
| `radios` | ✅ 真机上：WiFi 开着 / 走的是 WiFi / 飞行模式关。**撞出一个漏声明的权限**：读 `isWifiEnabled()` 要 `ACCESS_WIFI_STATE`，不加直接抛 SecurityException（已补上） |
| `brightness get` | ✅ 真机 auto=true, level=6；`set` 没授权时回 `no_write_settings` + 去哪开 |
| `screenshot` | ✅ 真机实测：1080×2424 / 314KB PNG 落到 `/workspace`，内容就是当时的屏幕（打开无障碍服务之后） |
| `agent tap` / `swipe` / `key` | ✅ 无障碍一开就跟着能用（实测 `home` 键）—— 这两件事本来就是同一只服务 |
| `alarm set --repeat weekdays` / `timer` | ✅ 交给系统时钟（`SKIP_UI` 只是请求）；`cancel --id` 明确说不支持 |
| `location current` | ✅ `adb emu geo fix` 之后拿到 `source:fresh` 的坐标（模拟器）；真机室内定不上，优雅降级到 `last_known_stale` + note（见下） |
| `location forward` | ✅ 真机（有 Google 服务）：“上海外滩” → 31.2402,121.4906；⚠️ 模拟器无 GMS 时回 `gcaa: UNAVAILABLE`（已翻成人话） |
| `contacts list/search/get` | ✅ 造了一个联系人（张三 / 13800138000 / 邮箱）走通；不存在 id → `not_found` |
| `calendar calendars/list/create/delete` | ✅ 定时事件、**全天事件**（UTC 午夜那条约定，见下）都对 |
| `sms list/stats` | ✅ `adb emu sms send` 发的短信读得到 |
| `notify` | ✅ |
| `speak` | ✅ 念完才返回 |
| 权限被拒 | ✅ 25s 后回 `permission_denied` + 「去哪开」；第二次瞬间返回 |
| 电脑上 `agent sms list` | ✅ 当场拒绝（exit 2）；`/api/device` 回 `unavailable`；`help` 里 0 条 |
| 电脑上 `agent weather` | ✅ 真联网拿到上海天气 |

两个坑记一下：

- **全天事件必须用 UTC 午夜**。CalendarProvider 按 UTC 解读全天事件的时间戳，
  给本地午夜的话事件会跑到前一天/后一天（实测：创建 `2026-11-05` 读回来是 `10-21`）。
  现在 `parseAllDayUtc` / `isoDateUtc` 成对处理，`EVENT_TIMEZONE` 全天时写 `"UTC"`。
- **`onPause` 清掉的 Activity 不能用来 startActivity**（上面第 5 节）。
- **`adb shell am force-stop` 会把无障碍服务一起停掉**。App 被 force-stop 之后处于
  「stopped」状态，系统**不会**自动把它的无障碍服务绑回来 —— 表现是 `agent screenshot`
  突然回 `no_accessibility`、点按也失灵。用户下次打开 App 就恢复（或者重写一次
  `enabled_accessibility_services`）。调试时踩过两次。

## 9. 系统不让 App 干的事（以及我们怎么处理）

| 想要 | 实际情况 | 我们做成什么 |
|---|---|---|
| 开/关 WiFi、蓝牙 | Android 10+ 起 `setWifiEnabled` 对普通 App 返回 false 且无效，`BluetoothAdapter.enable` 在 33+ 直接废弃 | `agent radios wifi` / `bluetooth` **打开设置页**，回报里写清「得人在这一页手动切」 |
| 改屏幕亮度 | 要 `WRITE_SETTINGS`，那是**特殊权限** —— 不是运行时弹框，只能用户手开 | `agent brightness set` 没授权时回结构化错误 + `agent brightness grant` 直接跳到那一页 |
| 截屏 | 普通 App 只能拉 MediaProjection，而它**每次**都要用户点一个“开始投屏”弹框 | 走已有的无障碍服务（`takeScreenshot`，API 30+，配置里声明 `canTakeScreenshot`）—— 也是 `agent tap` / `key` 同一只服务 |
| 控制其他 App 的音乐播放 | `AudioManager.dispatchMediaKeyEvent` 要 `MEDIA_CONTENT_CONTROL`（系统权限）；正道是拿**通知使用权**再走 `MediaSessionManager.getActiveSessions` | **没做**。多一个高频弹框 + 一个常驻监听服务，性价比不对（而且二维码广告类应用就是靠这个搞事情，用户很可能不想给） |
| 静音某个流 | —— | `agent volume mute --stream ring`（这个做得到） |
