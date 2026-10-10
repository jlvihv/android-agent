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

```
[system]  SYSTEM_PROMPT              ← 静态常量，零插值，永远不变
[system]  <环境>上下文#1</环境>        ← 第 1 轮生成后冻结进历史
[user]    U1
[assistant] ...
[tool]    ...（含操作后的最新界面）
[system]  <环境>上下文#2</环境>        ← 第 2 轮才追加
[user]    U2
```

三条规则：

1. **`SYSTEM_PROMPT` 是 `const &str`，不含任何 `{}` 插值。**
   不能有日期、时间、设备名、屏幕内容、当前目录 —— 任何运行期才知道的东西。
2. **动态内容作为独立消息，追加在历史末尾。** 生成一次就冻结，
   下一轮只往后加，不改前面。
3. **工具表顺序固定、序列化字节确定。** 工具定义也是可缓存前缀的一部分。

对应代码（`core/src/agent.rs`）：

```rust
pub const SYSTEM_PROMPT: &str = "你是运行在安卓手机上的操作助手……";  // 纯常量

// run_turn：每轮往历史**追加**上下文 + 用户输入
sess.messages.push(ChatMessage::System(context_block(core)));
sess.messages.push(ChatMessage::User(user_text.into()));

// stream_one：直接拼，不做任何插值
msgs.push(ChatMessage::System(SYSTEM_PROMPT.to_string()).to_openai()?);
for m in &sess.messages { msgs.push(m.to_openai()?); }
```

顺带一个细节：`ChatSession::provider_session`（作为 `x-opencode-session`
发出去）**开一次就不变**。OpenCode Go 用它做缓存路由，每次换 ID 等于每次冷启动。

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

`core/src/agent.rs` 里有 5 条测试专门守这个不变式：

| 测试 | 守什么 |
|---|---|
| `message_prefix_is_stable_across_turns` | **核心**：第 2 轮的消息数组必须以第 1 轮为前缀 |
| `system_prompt_is_a_frozen_constant_with_no_interpolation` | `SYSTEM_PROMPT` 里不能有 `{`，也不能出现 `rootfs` / `/data/` / `Activity` / `1080` 这类运行期内容 |
| `changing_the_screen_does_not_change_the_cached_prefix` | 环境变了，但前缀不能动 |
| `tool_schema_order_and_names_are_fixed` | 工具表顺序钉死（想改顺序必须先改测试） |
| `tool_schemas_serialize_byte_identically` | schema 序列化字节确定 |

`message_prefix_is_stable_across_turns` 是最重要的那条 ——
它模拟两轮对话，断言第二轮消息数组的前 N 项与第一轮逐字节相同。
任何人往 `SYSTEM_PROMPT` 里加插值、或重排历史，这条测试立刻红。

---

## 6. 顺带得到的副产品

- `data-usage` 事件会推到前端，所以 UI 可以直接显示 token 用量和缓存命中率。
- `finish_reason == "length"` 时会发 `data-warning`，
  否则输出被 `max_tokens` 截断看起来就像模型话说到一半。
- 每条工具结果里都带**操作后的最新界面**（`execute_tool` 的实现），
  这样界面新鲜度不依赖 system prompt —— 既保住了缓存，又让模型看到真实状态。
