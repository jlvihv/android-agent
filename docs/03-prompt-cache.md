# Prompt Cache：为什么提示词前缀一个字都不能变

> 实测日期：2026-10-10
> provider：OpenCode Go / `deepseek-v4.1-flash`

---

## 1. 这不是优化，是正确性

看 OpenCode Go 的价目表：

| 模型 | 输入 | **缓存读取** | 倍差 |
|---|---|---|---|
| DeepSeek V4.1 Flash | $0.30 / M | **$0.006 / M** | **50×** |
| GLM-5.3 | $1.40 / M | $0.26 / M | 5.4× |
| Kimi K3 | $3.00 / M | $0.30 / M | 10× |
| Qwen3.8 Max | $2.00 / M | $0.25 / M | 8× |

缓存是按**前缀**命中的。只要消息数组的第一个字节变了，整个对话的缓存全部失效，
这一轮的输入就按全价计费。

agent 场景尤其致命：一个回合可能有 5–8 次 LLM 调用，每次都要重发整段历史
（含工具输出、UI 树快照）。如果前缀不稳定，成本直接乘 50。

---

## 2. 我们踩过的坑

第一版实现是这样的：

```rust
// ❌ 错的
async fn stream_one(...) {
    let system = system_prompt(core);   // ← 里面塞了「当前界面快照」
    let mut msgs = vec![];
    msgs.push(ChatMessage::System(system).to_openai()?);  // ← 而且是第一个元素
    for m in &sess.messages { msgs.push(m.to_openai()?); }
    ...
}
```

`system_prompt()` 会把当前屏幕的 UI 树拼进字符串。于是：

- 用户点了一下 → 屏幕变了
- 下一轮 LLM 调用 → system prompt 变了
- **system 是消息数组的第一个元素 → 整段历史的缓存全废**

一个「打开设置再点一下」的操作就能让缓存归零。

---

## 3. 正确的形状

**系统提示词就是历史的第一条**，请求体 = 历史原样，不拼任何东西：

```
[system]  SYSTEM_PROMPT              ← 建会话时写入历史，之后不动
[user]    U1
[assistant] ...
[tool]    ...（read 的正文 / bash 输出 / edit 的 diff）
[user]    ...（read 读到图时的图片 carrier，见下）
[user]    U2
```

> **图片 carrier**：`read` 读到图片时，tool 消息后面会紧跟一条 `user`
> 消息，里面是 `image_url`。OpenAI 的 tool 消息内容只能是字符串，图片没
> 别的地方可挂。它由历史推导、字节确定（图片从盘上重读），所以照样是前缀
> 的一部分 —— 后面的轮次会原样重发同一份字节，缓存命中不受影响。
>
> 这是 **Chat Completions** 的绕法。**Responses API** 不需要：图片直接放在
> `function_call_output.output` 的内容块数组里（`build_responses_input`），
> 更干净、也不多一条消息。两条协议编码同一份历史，前缀不变式一致。

三条规则：

1. **`SYSTEM_PROMPT` 是 `const &str`，不含任何 `{}` 插值。**
   不能有日期、时间、设备名、屏幕内容、当前目录 —— 任何运行期才知道的东西。
2. **历史只追加，不注入任何“每轮重发”的东西。**
   需要环境信息（手机存储挂没挂、现在屏幕是什么）就让模型自己查：
   一次 `ls` 的开销远小于每轮重发一段动态文本。
3. **工具表顺序固定、序列化字节确定。** 工具定义也是可缓存前缀的一部分。

对应代码（`core/src/agent.rs`）：

```rust
pub const SYSTEM_PROMPT: &str = "你是运行在安卓手机上的 Agent。……";  // 纯常量

// run_turn：补上当前提示词（没出现过才追加），再追加用户输入
ensure_system_prompt(sess, SYSTEM_PROMPT);
sess.messages.push(ChatMessage::User(user_text.into()));

// build_messages：历史原样；唯一的例外是图片 carrier，排在连续 tool 消息之后
build_messages(sess, supports_images, supports_files)
// Responses API 走同一个历史的另一种编码（build_responses_input），
// 工具结果里的图片直接内联，不需要 carrier
```

因此请求体长度 == 历史长度（加上图片 carrier）。`the_request_is_the_history_verbatim`
和 `message_prefix_is_stable_across_turns` 守着这一点。

### 3.1 改了系统提示词怎么办：**追加，不替换**

这是把提示词放进历史的最大好处：**升级提示词不会动老会话的前缀**。

```
[system]  旧 SYSTEM_PROMPT          ← 保留
[user]    U1
[assistant] ...
[system]  新 SYSTEM_PROMPT          ← ensure_system_prompt 追加在末尾
[user]    U2
```

- 老前缀 `[旧提示词, U1, A1, …]` **原封不动** → 缓存照命中；
- 新提示词作为一条 `system` 消息落在**新用户输入之前**；它一旦进历史就固化了，
  下一轮开始就在缓存里；
- 同一条提示词不会重复追加（`ensure_system_prompt` 先查历史里有没有）；
- 历史里没任何提示词的老会话（比如以前存的）会自动被补上。

推论：**提示词变了也不能轮换 `provider_session`** —— 那正是要保住的缓存路由。

代价：历史里可能同时存在两条 system 消息，靠“后面那条更新”来生效。
如果哪天发现模型不认，就把追加的那条包上标记（如
`<系统提示词更新>…</系统提示词更新>`）—— 反正它进历史后就不再变。

### 3.2 缓存路由 ID

`ChatSession::provider_session`（作为 `x-opencode-session` 发出去）
**开一次就不变**。OpenCode Go 用它做缓存路由，每次换 ID 等于每次冷启动。

唯一该换的时机是「前缀的语义彻底断了」：历史被清空（`clear()`）、
provider / model 换了（`/api/llm`）。这两种情况下代码会
`rotate_provider_session()`，并且会落盘 —— `provider_session` 的 upsert
原来漏了这一列，换过之后重启又会用回旧 id。

### 3.3 两套缓存字段谁优先

取命中数时要在
`prompt_tokens_details.cached_tokens` 和 `prompt_cache_hit_tokens` 里取**大**的那个。
用 `.or()` 的话，只要前者是 `Some(0)`（有些网关冷启动会填 0 而不是省略字段），
后者报的真值就被吞掉，日志显示 0% —— 而这是唯一能证明缓存生效的数字。

---

## 4. 实测结果

4 个连续回合（每回合触发 2 次 LLM 调用），服务端日志：

```
prompt cache 命中  0.0%（cached=0    prompt=1334 completion=71 reasoning=25）   ← 冷启动
prompt cache 命中 88.0%（cached=1280 prompt=1454 completion=2  reasoning=0）
prompt cache 命中 84.2%（cached=1280 prompt=1520 completion=113 reasoning=34）
prompt cache 命中 90.0%（cached=1536 prompt=1706 completion=17 reasoning=15）
prompt cache 命中 79.9%（cached=1408 prompt=1763 completion=60 reasoning=8）
prompt cache 命中 88.9%（cached=1664 prompt=1872 completion=2  reasoning=0）
prompt cache 命中 85.1%（cached=1664 prompt=1955 completion=52 reasoning=0）
prompt cache 命中 93.4%（cached=1920 prompt=2056 completion=2  reasoning=0）
```

**第一次 0%，之后稳定在 80–93%。** 这正是前缀单调增长该有的样子。

### 精简的收益：一个具体对比

同一个问题（「只回一个字：好」）在**同一台手机**上：

| | prompt tokens |
|---|---|
| 精简前（7 个工具：5 个 `device_*` + 每轮追加整棵 UI 树） | **24995** |
| 精简后（2 个工具、提示词 630 字符、不追加任何东西） | **1074** |

差 23 倍。这里省下来的每一分都在**冷启动那一轮**（0% 命中，全价）和上下文窗口上 ——
纯聊天这种不需要看界面的回合，之前有 96% 的输入是白给的。

### 怎么读这些数字

用量字段有**两套写法**，都接上了（`core/src/agent.rs` 的 `Usage`）：

| 风格 | 字段 |
|---|---|
| OpenAI 系 | `prompt_tokens_details.cached_tokens` |
| DeepSeek 系 | `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` |

实测 OpenCode Go 的 `deepseek-v4.1-flash` 走的是 **OpenAI 系**。
`prompt_cache_hit_tokens` 是 `null` —— 如果只按 DeepSeek 的字段名解析，
就会误以为"这个 provider 不支持缓存"。

另外 `completion_tokens_details.reasoning_tokens` 也值得看：
DeepSeek 系的推理 token 按 **output** 计费（$1.20/M），
实测一次简单问答就能产生 25–136 个推理 token。

### 怎么打开用量上报

```rust
let mut req = serde_json::to_value(&req)?;
req["stream_options"] = json!({ "include_usage": true });
```

`ChatCompletionStreamOptions` 没有 `derive Default`，逐字段构造很脆；
先序列化成 `Value` 再补键更稳（`create_stream_byot` 的 `T0` 只要 `Serialize`）。

---

## 5. 守住它的回归测试

`core/src/agent.rs` 里有 9 条测试专门守这些不变式：

| 测试 | 守什么 |
|---|---|
| `message_prefix_is_stable_across_turns` | 第 2 轮的消息数组必须以第 1 轮为前缀 |
| `a_prompt_change_is_appended_and_keeps_the_prefix` | **改了系统提示词也不动老前缀**：新提示词追加在末尾，第一条不替掉，也不重复追加 |
| `full_request_body_is_prefix_stable_across_turns` | 不只比 `messages`，整份请求体（含 `tools` / `stream_options`）跨轮都得是前缀关系 |
| `system_prompt_is_a_frozen_constant_with_no_interpolation` | `SYSTEM_PROMPT` 里不能有 `{`，也不能出现 `rootfs` / `/data/` / `Activity` / `1080` 这类运行期内容 |
| `the_request_is_the_history_verbatim` | 请求体长度 == 历史长度：不拼、不前置、不插上下文块 |
| `tool_schema_order_and_names_are_fixed` | 工具表顺序钉死（想改顺序必须先改测试） |
| `tool_schemas_serialize_byte_identically` | schema 序列化字节确定 |
| `browser_is_only_reachable_through_the_sandbox_cli` | 浏览器只能从沙箱那个门进（工具表里不能有 `browser`，提示词里必须有 `agent browser`） |
| `usage_understands_both_cache_field_styles` | 两套缓存字段都认，同时在时取大的那个 |

`full_request_body_is_prefix_stable_across_turns` 是硬的那条：它不断言中间函数，
而是直接断言**构造好的请求体**（`build_request()`，和 `stream_one` 走同一条路）
跨轮的前缀关系 —— 工具表、字段省略行为、请求体字节序全在里面。

---

## 6. 顺带得到的副产品

- `data-usage` 事件会推到前端，所以 UI 可以直接显示 token 用量和缓存命中率。
- `finish_reason == "length"` 时会发 `data-warning`，而且当轮的工具调用**全部作废**
  —— 截断的 `arguments` JSON 很可能是半个对象，拿坏参数去执行比报错危险得多。
- 环境（沙箱可不可用、手机存储挂没挂）**不进 prompt**：它在 `SYSTEM_PROMPT` 里只有
  一句“不确定就先 `ls /sdcard`”。早先版本每轮追加一个 `<环境>` 消息，再往前还塞过
  当前界面的 UI 树 —— 都是几百行文本每轮重发，纯烧 token，全删了。

---

## 7. 思考内容要回传

带工具的多步回合里，**每一步都要把之前各步的思考内容原样放回请求**，否则模型每一步
都看不到自己上一步想了什么 —— 不报错，只是更容易前后不连贯、每步重想一遍（更慢、
思考 token 更多）。DeepSeek 官方 API 在思考模式 + 工具调用时**强制**要求，不传直接
400；有的网关不强制，于是这件事可以悄无声息地坏很久（我们就是）。

两条协议各传各的：

- **Responses API**（OpenCode Go 的 `deepseek-flash` 实际走的就是它）：输出里的
  思考条目（`type: "reasoning"` 的输出项）**原样**存进历史
  （`ChatMessage::Assistant::reasoning_items`），下一次请求原样放回 input，排在那一步
  的正文和工具调用前面。请求带 `include: ["reasoning.encrypted_content"]`；只回传带
  `encrypted_content` 的条目 —— 我们发的是 `store: false`，光有 id 的条目 OpenAI 会去找、
  找不到就 400。
- **Chat Completions**：assistant 消息带上 `reasoning_content`（DeepSeek 系的字段）。
  不返回思考内容的模型历史里本来就是空的，字段自然不出现。

实测（2026-10-13，OpenCode Go / `deepseek-flash`）：网关给的思考条目形状是
`{type, id, status, summary: [], content: [{type: "reasoning_text", text}], encrypted_content}`；
原样回传 + `include` 都被接受。验证方法：让模型**只在思考里**随机想一个 6 位数，调一次
工具，再问它那个数 —— 回传之后它能原样答出来，第二步的输入 token 也多出了思考那一段。
回传会让老会话的前缀变一次（多了思考条目），之后照常命中缓存（实测 98%+）。
