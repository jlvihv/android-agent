# 特权命令：`agent shizuku`（以 adb / root 身份跑系统命令）

> 2026-10-20。**模拟器（API 35，Shizuku 以 root 起）和真机（Pixel 10 / Android 17，
> Shizuku 以 adb 起）都验通**：`service status` 四态、`exec` 分别拿到 `uid=0(root)`
> 和 `uid=2000(shell)`、超时/失败/元字符都按约定回话。

## 1. 一句话

`agent sms` / `agent contacts` 那一批用的是**这个 App 自己的身份**（uid = 应用）
加 Android 公开 API。`agent shizuku` 用的是**另一个身份** —— 通过 Shizuku
（或 Sui / AXManager）借到 adb shell（uid 2000）甚至 root（uid 0），
跑的是 `pm` / `am` / `settings` / `dumpsys` / `input` 这些命令行工具，
**和开发者插着数据线 `adb shell` 敲的是同一套**。

两批能力互补，不重叠：

|        | `agent contacts` 那批    | `agent shizuku`                                    |
| ------ | ------------------------ | -------------------------------------------------- |
| 身份   | 应用自己                 | adb（2000）/ root（0）                             |
| 依据   | 运行时权限 + 公开 API    | 命令行工具                                         |
| 能做到 | 读联系人 / 发短信 / 定位 | 停别的 App / 改系统设置 / 模拟点击 / 看 dumpsys    |
| 做不到 | 碰别的 App、改系统设置   | 没有 App 自己的 API（要发短信得走 `am`/`content`） |

## 2. 分层：表在 Rust，判断也在 Rust

```
沙箱里的 agent shizuku …     ← cli.rs 按机器生成，手机上才有
        ↓ POST /api/shizuku（带 token）
Core::shizuku()              ← spawn_blocking + 300s 超时 + reply_text 收敛
        ↓ UniFFI callback
ShizukuHostImpl.kt           ← 只做三件事：state() / run(argv) / requestPermission()
        ↓
ShizukuManager.kt            ← binder 状态机 + 反射 Shizuku.newProcess
```

**参数校验、dumpsys 解析、排版全在 `core/src/shizuku.rs`**，不搬进 Kotlin。
理由是那三样都是**判断**，而判断放在 Rust 里才能写单测（喂一段真 dumpsys 文本、
断言解析出来的字段）；放在 Kotlin 里就只能靠真机手点。壳那边只剩「拿身份、跑命令」。

`run()` 的三种回包形状（`shizuku::ShizukuApi::run`）：

```json
{"ok":true, "text":"…"}                            原样打（exec 的 stdout、ls）
{"ok":true, "data":{…}}                             核心用 plain::render 排成表格/键值
{"ok":false,"error":{"code":"…","message":"…"}}     错误：…（code）→ CLI 非 0 退出
```

纯文本是**唯一**输出形式（没有 `--format json|csv`、没有 `-q`）：排版只在核心侧
一份，两套输出规范必然各自漂（[docs/10](10-tools-vs-sandbox.md) 第 3 条踩过）。

## 3. 状态机：binder 优先

判断「能不能用」**只看 binder**，不看装了哪个管理器 APK：

```
binder 活着 + 已授权  → READY
binder 活着 + 没授权  → NEED_PERMISSION   （去授权）
binder 没了 + 装了 APK → NOT_RUNNING        （去启动）
binder 没了 + 没装     → NOT_INSTALLED      （去装）
```

顺序就是全部逻辑。反过来写（先查包名）会把 **Sui 判死** —— Sui 是 Magisk/KernelSU
模块，根本没有 APK，先在包名那一步 short-circuit，活着的 binder 一次都没被问过。
`isSui` 只用来改文案，不参与任何判断。

四条路径都在模拟器上现跑过（授权 → 杀服务 → 重开）。`decideState` 是纯函数，
`android/app/src/test/.../ShizukuManagerTest.kt` 覆盖了这四条。

## 4. 执行：反射调隐藏 API

`Shizuku.newProcess(argv, env, cwd)` 不在公开 SDK 里，但它是官方 manager 自己
用的那条路，返回的 `ShizukuRemoteProcess` 本身就是 `java.lang.Process`。三个签名
依次试（3 参数 / 2 参数 / 1 参数，各版本不一样），都拿不到就回一句明确的错误 ——
**不装成功**。

两个踩过的坑，代码里都留着注释：

1. **`RemoteProcess.waitFor(long, TimeUnit)` 在 13.x 上会抛**
   `IllegalArgumentException("process hasn't exited")` 而不是阻塞。所以退化成轮询
   `exitValue()`。
2. **超时必须先杀再收尾**。反过来的话收尾要等 2 秒，而进程还在往里写 ——
   实测 `sleep 5; echo done` 带 `--timeout-ms 2000` 会回一句「超时」外加
   `done`，等于骗人。

`exitCode` 沿用 coreutils 约定：`124` 超时、`143` 被杀、`126` 没授权。

## 5. 不审批

这套命令**直接执行**，没有「挂起等用户点确认」那一层 —— 和别的工具一致
（铁律 7：服务端不保存「挂起等回复」的状态）。代价写在明处：

- `package uninstall`、`settings set`、`file rm`、`permission grant` 一次误判
  就能改坏东西；
- 唯一的防线是**每组 help 里那句「会改什么」**（`shizuku::GROUPS[].danger`），
  那是模型唯一看得见的风险提示。

唯一绕不过去的授权是 **App → Shizuku** 的那一次：它不是一个系统运行时权限
（不能走 `AgentPermissions` 的弹框桥），而是 `Shizuku.requestPermission(code)`
拉起管理器界面，必须在主线程且有 Activity。所以它单独是 `PermissionCenter` 里的
一项（`Kind.SHIZUKU`），点一下按状态分派：没就绪 → 打开管理器 App，
没授权 → 拉授权界面。

## 6. 只有手机上有

`cli.rs` 里 `on: PHONE`：桌面上那份 `agent` 脚本里**根本没有** `shizuku` 这个
case（宿主模式也没有 —— 不打 adb），模型在电脑上看不到它，也就看不到
「这台机器上没有」这层区分。有测试守着（`cli::tests`）。

## 7. 验收现状

| 项                                       | 状态                                                                                        |
| ---------------------------------------- | ------------------------------------------------------------------------------------------- |
| `service status` / `ping`                | ✅ 模拟器：READY / NOT_RUNNING / NEED_PERMISSION 三态现场跑过                               |
| `exec`（含管道、重定向、`;`）            | ✅ `dumpsys battery \| grep level`、`getprop` 都对                                          |
| 失败带输出、非 0                         | ✅ `错误：…（OPERATION_FAILED）`，CLI 据此非 0 退出                                         |
| 超时 124                                 | ✅ 且**不再带**超时之后才产生的输出                                                         |
| Rust 单测                                | ✅ 450 passed（13 条 shizuku 相关）                                                         |
| Kotlin 单测                              | ✅ `decideState` 五条                                                                       |
| uid 2000（真机 / 无线调试启动）          | ✅ Pixel 10 / Android 17：`uid=2000(shell) ... context=u:r:shell:s0`                        |
| **沙箱里敲 `agent shizuku`**（最后一跳） | ✅ Pixel 10 真机：`agent shizuku exec id` → `uid=2000(shell)…`                              |
| P1 的 11 组（真机）                      | ✅ battery / top / info / display / package info / settings 读写删 / input tap / 各种参数错 |
| **release 包**（R8 混淆后）              | ✅ 签名与 debug 一致、覆盖安装不丢数据；`exec id` 仍拿到 uid 2000                           |
| `&&` 链在失败处断开（CLI 非 0）          | ✅ `exec echo A && exec ls /nope && echo 不该出现` → 只出 A + 错误，exit 1                  |

## 8. 组（P1 已做完）

11 组：`service` `device` `package` `settings` `activity` `input` `display`
`user` `notification` `file` `exec`。表在 `shizuku::GROUPS`（**help、提示词、
校验、分发都从这一张表生成**）。

分成两类：

- **能结构化的**（`device battery/top/info`、`package list/info/permissions`、
  `display list`、`user list`、`notification list`、`settings list`）：
  解析在 `core/src/shizuku_parse.rs`，输出是短字段/表格。
- **只有一句回执的**（`uninstall` / `input tap` / `settings set` / `file rm`）：
  跑完说「已卸载 com.x」，不把 `Success` 这种机器话原样吐出来。

**参数在 `plan()` 里校验，拼命令也在那儿**：两者本来就是一件事。给错参数要在
**发出去之前**拦住 —— 静默拼出一条别的命令，模型会对着"成功"的输出继续往下错。

**值一律 `shq()` 转义**：模型给的值可能带 `;`、反引号、`$()`，而这是 adb 身份。
不转义等于命令注入。

## 8.1 解析器：解析不出来就交回原文

`shizuku_parse` 里每个 `parse_*` 返回 `Option`。拿不到关键字段时**不做部分解析、
不编默认值**，直接 `None` → 退回原始 `exec` 输出 + 一句「这台机器上的输出格式认不出来」。

理由：解析器坏了的时候，「看起来成功但字段是空的」比「直接给原文」糟得多。
模型看到 `电量: -1` 会当成真的没电；看到 `dumpsys battery` 原文，它自己就能读出来，
而且立刻知道解析器没跟上这台机器。

样本在 `core/tests/fixtures/shizuku/*.txt`，**都是 Pixel 10 / Android 17 上抓的真文本**
（battery / users / display / packages / package-info / notification / netstats /
activities / getprop）。解析器的单测直接喂这些文件 —— 换系统版本时把这批样本重抓一遍，
测试会立刻告诉你哪条格式变了。

## 9. 输出要多短

模型每一轮都要重读整段上下文，所以**工具的每一行输出都是持续成本**。这套命令的
输出按「一行能说完就一行」来定，下面每条都是实测改出来的，不是感觉：

| 改前                                                             | 改后                             | 为什么                                                                |
| ---------------------------------------------------------------- | -------------------------------- | --------------------------------------------------------------------- |
| `service status` 5 行（状态/来源/版本/身份/uid）                 | 1 行 `Shizuku 13，adb uid=2000`  | 「身份: adb」和「uid: 2000」是同一件事；能跑通时「状态: READY」是废话 |
| 同样内容另有一套 5 行 JSON 排版                                  | 只有这一行                       | 两条命令给了两套状态格式，本身就是冗余                                |
| `错误：ls: /nope: …（OPERATION_FAILED）`                         | `错误：ls: /nope: …`             | 命令原文已说清；这个码零信息量                                        |
| `错误：命令失败（exitCode=1，没有任何输出）（OPERATION_FAILED）` | `错误：exitCode=1，没有输出`     | 三个括号一个意思                                                      |
| `OK Shizuku 就绪（adb uid=2000）`                                | `OK`                             | ping 是给脚本判断的                                                   |
| help 里还有一句「不审批：敲了就跑」                              | 删（信息在组名和「会改东西」里） | 元信息，不是能力                                                      |

保留的两处「不精简」是刻意的：

1. **`TIMEOUT` / `PERMISSION_DENIED` / `KILLED` 三个码留着**。它们意味着
   「换参数也没用，得改条件」—— 那正是模型需要知道、否则会反复重试的东西。
   其余码一律不印。
2. **`会改东西：等于 adb shell…` 留在 help 里**。不审批的代价靠这句话对冲
   （§5）。删了它，「不审批」就只剩风险没有提示 —— 我试过删，然后退回来了。

## 10. 大输出必须拦

`agent shizuku exec` 的输出直接进模型上下文，而 `dumpsys` 家族**动辄几 MB**
（实测 `dumpsys activity` = 2.07 MB / 28043 行）。模型很自然会写
`agent shizuku exec "dumpsys activity | head"`，但只要漏一次，一轮就把上下文吃满。

所以 `reply_text` 在返回前过一遍 **`linux::truncate_for_model`** —— 和 `bash`
同一组常量、同一条「首尾各留 100 行 + `[省略 N 行]`」规则。不另立一套的理由和
输出格式一样：两个入口给两种上限，模型会学到错的东西。

```
$ agent shizuku exec "dumpsys activity"
（前 100 行）
[省略 27942 行]
（后 100 行）
（输出太长已截断 —— 自己加过滤重跑，比如 `| head`、`| grep 关键词`）
```

2.07 MB → 12 KB，并且告诉模型下一步怎么写。`| grep` 之后就是完整输出
（`dumpsys activity | grep -c ActivityRecord` → `48`）。

**没有**像 `bash` 那样 spill 到 `/tmp` 让模型再去读：完整输出在特权侧，
落盘要再走一趟特权调用；而"加个过滤重跑"对它来说是一句话的事，比多一次
读写文件更短。

## 11. 一个只有真机能撞到的 bug

提示词和 help 里写着「`agent shizuku help` 看每一组」—— 但 `validate` 只认
`service` / `exec`，**`help` 不在里面**。单测没发现（我没给 help 写用例），
在真机沙箱里敲才暴露：

```
$ agent shizuku help
错误：`agent shizuku` 没有「help」这一组（有：service / exec）
```

教训：**凡是提示词里承诺的入口，都得有一条测试真的敲它**。现在
`help_is_a_real_subcommand` 和 `every_usage_mentioned_in_docs_is_runnable`
两条守着。

## 12. R8 混淆会打断反射（装 release 必看）

`ShizukuManager.newProcess()` 调的是
`Shizuku::class.java.getDeclaredMethod("newProcess", …)` —— **按字符串找方法**。
R8 把方法改名成 `a()` 之后，这个调用抛 `NoSuchMethodException`，表现是
「debug 包好的、release 包拿不到 adb 身份」。

`app/proguard-rules.pro` 里为此留了：

```proguard
-keep class rikka.shizuku.** { *; }
-keep class rikka.sui.** { *; }
-keep class moe.shizuku.** { *; }
```

验证办法（不用真机跑）：把 release APK 的 dex 抽出来 `strings | grep` 那几个名字 ——
`newProcess` / `pingBinder` / `checkSelfPermission` / `addBinderReceiverSticky` 都还在，
`Lrikka/shizuku/Shizuku;` / `Lrikka/sui/Sui;` 也还在，就说明 keep 生效了。
（只 grep `^rikka\.shizuku\.Shizuku$` 会漏 —— dex 里存的是类型描述符
`Lrikka/shizuku/Shizuku;`。）

另外 release 包 **不是 debuggable**，`run-as` 读不到 `files/token`。调试时用另一条路：
`adb logcat | grep 'agent token'`（`AgentApp.onStart` 打的那行），
或者 `/sdcard/Android/data/<pkg>/files/port`（那份谁都读得到）。
