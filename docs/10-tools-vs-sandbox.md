# 工具表为什么只有三个（`read` / `bash` / `edit`）

> 数据来自 2026-10-13 的真机会话

---

## 1. 判据

**能在 shell 里说清的能力，不做成工具。** 模型只需要学一个东西：shell。
其余能力都是沙箱里的 `agent xxx` 子命令（`agent help` 列全）。

为什么：工具调用**一次只做一个动作**，而一条 bash 能连做多个 ——
省下的是**整轮 LLM 请求**（每轮都要把整个上下文再发一遍）。多步页面操作、
「搜完再挑一条读全文」这类事，差距就是几倍。

这条判据不是审美，是量出来的（下一节）。

## 2. 模型自己选了沙箱那条

真机 23 个会话、377 次工具调用：

- `agent browser` 56 次 vs `browser` 工具 26 次；
- 一个长会话里是 **45 : 3** —— 它跑过一次 `agent help` 发现有沙箱命令之后，
  **再没回过头**；
- 带 `agent browser` 的 24 条 bash 里，**17 条塞了 2~6 个动作**（最多 6）；
- 它甚至开始拿 bash 当胶水：变量拼沙箱 URL、循环、`| jq`、`| head`。

```
$ agent browser navigate https://example.com && agent browser get_backbone 3 && agent browser click '#x'
```

一次调用做完三步 = 一轮 LLM 请求；用工具做就是三轮。

## 3. 两条路并存的代价

1. **前缀体量**：`browser` 一条 2418 字，占当时整张工具表（4495 字）的 **54%**。
   删掉 `browser` + `search` 之后整表 **1785 字**。
   （工具表是可缓存前缀的一部分，见 [docs/03](03-prompt-cache.md)。）
2. **两份动作表各自演化 → 静默丢参数**。工具和 Kotlin 那边加了多标签，CLI 的 `case`
   没跟上，于是 `agent browser new_tab "file:///…"` 走进兜底分支
   **把 URL 吃掉**，开出一只空白标签，模型以为页面开了、对着白屏截了三张图。
   所以 CLI 对每个动作的参数个数上下限都校验（少给/多给都报错）。
3. **模型要在两个门之间犹豫**：同一个会话里两条腿都用过（先用工具，发现沙箱
   那条之后才统一）。

## 4. 那为什么这三个还是工具

| 工具 | 为什么它必须是工具 |
|---|---|
| `read` | 不是「读文件」这么简单：文本要按行截断 + 给续读提示（`offset` / `limit` / `numbered` / `totalLines`），图片要走**缩放 → 落盘 → 作为 image 内容块发给模型**那条管线。做成 shell 命令 = 把这两套规则塞进 shell 让模型每次重新学 |
| `bash` | 唯一的入口，其余能力都从它走 |
| `edit` | 按原文精确替换 + 返回 unified diff + 原子写。用 shell 做就是 `sed` / `python3` 拼字符串，错一次毁一个文件；schema 逼模型把 `oldText` 给全 |

一句话：**这三个的 schema 本身就是提示词**（在教模型怎么用）；而浏览器 / 搜索 /
设备操作是「动作名 + 参数」，翻译成 shell 更自然。

## 5. 一个能力只能有一个门

加了沙箱命令就把工具删掉，别留两条。守这条规矩的测试（`core/src/agent.rs`）：

| 测试 | 守什么 |
|---|---|
| `tool_schema_order_and_names_are_fixed` | 工具表的顺序与名单钉死（`read` / `bash` / `edit`） |
| `browser_and_search_are_only_reachable_through_the_sandbox_cli` | 表里不能有 `browser` / `search`，也不能有 `full_page` / `num_results` 这些只属于它们的参数；提示词里必须有 `agent browser` / `agent search` |
| `agent_cli_braces_variables_before_multibyte` | CLI 的写法陷阱：`$cmd（` 在 busybox ash 里会被当成变量名（见下） |

## 6. 再加一个能力时怎么做

1. **沙箱命令**：`core/src/linux.rs` 的 `AGENT_CLI` —— 加 `case` 分支 + 参数个数
   校验 + `usage()`（用法要写进脚本头部的注释，`agent help` 就是打印它）。
   ⚠️ 变量后面紧跟中文标点必须写 `${var}`：沙箱里是 busybox ash，它不认 UTF-8，
   会把全角字节当成变量名的一部分，`set -u` 下直接「unbound variable」退出 1。
2. **核心那层**：能直连的接口（`/api/xxx`），并且**把「给模型看的文字」在核心侧
   格式化好**（响应里的 `output` 字段），别让 CLI 和工具各写一份 ——
   各写一份就会漂（`format_hits` 就是这么搬到 `search.rs` 的）。
3. **提示词**：`compose_prompt`（`core/src/prompt.rs`）里那行 `agent` 命令补一句。不补的话模型得自己跑
   `agent help` 才发现，白花两轮。
4. **量一下体量**：`cd core && cargo run -q --example dump_prompt`；
   线上那份看 `GET /api/debug/tools`（见 [docs/07](07-debugging.md)）。
5. **老会话回放**：web 里给老工具名留个 `case`（`MessageList.svelte` /
   `toolview.ts` 里的 `browser` / `search` 就是干这个的），历史得照旧显示成人话。

## 7. 设备操作为什么一直在沙箱里

设备操作（读界面 / 点 / 输入 / 按键 / 启动应用）**不是工具**，在沙箱里
（`agent command`，见 [docs/02](02-linux-sandbox.md)）。浏览器和搜索后来也是这么归队的。

以后要重做「读 UI 树」也是加 `agent` 子命令，不是加工具。
