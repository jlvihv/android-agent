# 联网搜索：DeepSeek 的 Search API

> 数据日期：2026-11-26（价格与限额以 DeepSeek 官网为准）

---

## 1. 一个端点，一个 key

`core/src/search.rs`：

| 端点                                            | 凭据                                       | 限流           | 计费                   |
| ----------------------------------------------- | ------------------------------------------ | -------------- | ---------------------- |
| `POST https://api.deepseek.com/v1/tools/search` | `Authorization: Bearer <DeepSeek API Key>` | 账号级 QPS 100 | 5 元 / 1000 次成功请求 |

**搜索没有自己的配置项。** key 只认「模型提供商 → DeepSeek」那一行（`providers` 表里
`deepseek` 的 `api_key`），而且**必须是 platform.deepseek.com 的官方 key**：

- **不借用别家的 key**。OpenCode Go / Zen 看着也是 DeepSeek 系模型，但它们的 key
  打 `api.deepseek.com` 是 **401**（实测 `oc_sk_…` → `Your api key … is invalid`）。
  中转站同理：搜索端点不在它们代理的范围内。所以平时聊天走 OpenCode Go 的人，
  这一栏仍然显示「未配置」，要单独填一个官方 key。
- **不遍历 `providers` 找「任意一个能用的」**。用一个打不通的 key 换回 401、白跑
  一轮，比「明确说没配」糟得多。
- **没有环境变量、没有单独的搜索配置**（以前那个 `exa_api_key` 已经删了）。

请求体：

```json
{ "query": "…", "count": 5, "need_content": false }
```

响应 `{ "results": [...] }`，每项有 `title` / `url` / `snippet`（短摘要，给前端）/
`summary`（600～800 字长摘要，给模型读）/ `site_name` / `date` / `rerank_score`（≥1.5
算优质）/ `authority`（≥3 算极权威）。我们取 `summary`，空了退回 `snippet`；标题空了
退回 `site_name`，再退回域名。

摘要**原样进上下文**，不做任何 Markdown / 实体清洗 —— 模型自己认识 Markdown，代码块和
`C#` 这类内容被"清洗"只会被改坏（第一版全局删 `#` 就把 `C#` 删成了 `C`）。`report` 只做
两件事：按字数截断（成本），以及把续行加上缩进（排版）。

`query` 支持搜索语法（原样透传给搜索服务，我们不做解析）：`site:` / `-site:` / `-词` /
`"精确短语"` / `lang:zh,en` / `after:2025-01-01` / `before:`，可以组合。

## 2. 为什么把 Exa 换掉

以前默认走 Exa 的**公开 MCP 端点**（`https://mcp.exa.ai/mcp`，不需要任何凭据，服务端
拿 Exa 自己的 key 去打，按 **IP** 限流 2 次/秒、50 次/天）。那是 Exa 的获客漏斗，好处是
零配置，代价是：

- 额度按 IP。运营商 CGNAT 下那 50 次是**同一个出口 IP 上所有人共用**的，随时 429，
  而且不是我们的 bug —— 错误文案只能写「等一下 / 去配 key / 改用 browser」。
- 免费档只给基础搜索，`web_search_advanced_exa` 和 deep researcher 都要自带 key。
- 端点和工具名随时可能变，没有 SLA。

换成 DeepSeek 之后：**已经有 key 了**（对话在用），按次计费但便宜（5 元/1000 次，无结果
不计费），限流按账号而不是按出口 IP，而且 `count` 只影响条数不影响价格。

**代价说清楚**：现在没 key 就搜不了（以前「零配置勉强能用」）。所以缺 key 的报错必须
给出路：去设置里填 / 用 `agent browser` 打开搜索引擎 / `curl` 已知 URL。见第 4 节。

## 3. 手机上必须知道的三个边界

1. **402 ≠ 401**。401 是 key 错/失效（去设置里重填），402 是账号余额不足（充值）。混成
   一句「搜索失败」的话，用户在手机上根本没法判断该干什么。429 也单独一条文案。
2. **没有 key 就没有搜索**。这是刻意换来的确定性：不再有「有时候能搜有时候不能、原因
   在别人家」的状态。
3. **正文不拉**（`need_content: false`）。十篇全文进历史 = 进提示词缓存 = 按 token 烧钱，
   而且 DeepSeek 的 Search 只要返回了结果就计费，拉正文只是让上下文变大，不省钱。

## 4. 配 key

桌面和手机一样：设置 → 模型提供商 → DeepSeek 里填一个**官方 key**
（[platform.deepseek.com/api_keys](https://platform.deepseek.com/api_keys)，写进
`providers` 表），**没有环境变量那条路** —— 配置只有一个来源，不然「界面上填的」和
「shell 里带的」会互相打架。

设置页的「联网搜索」一栏现在**只有状态，没有输入框**：显示走哪家、配没配、key 末尾几位。
想用 curl 看一眼：

```sh
curl -sS -H "x-agent-token: <token>" http://127.0.0.1:18787/api/websearch/config
# {"provider":"DeepSeek","configured":true,"keyHint":"…c7f1"}
```

以前那个 `POST /api/websearch/config {"exa_api_key": "…"}` **已经删了**。库里如果还留着
老的 `exa_api_key`（`meta` 表），那是死数据，不读也不清 —— 想清就
`DELETE FROM meta WHERE k='exa_api_key'`。

## 5. 输出为什么这么小气

搜索结果进历史 = 进提示词缓存前缀 = **按钱算**。所以每条片段截 400 字、
整段封顶 4000 字。排版只在 `core/src/search.rs` 的 `report` 一处做 —— 连抬头
`3 条 · DeepSeek` 和正文一起出（分两处拼就会漂：以前抬头在 `http.rs`、正文在
`search.rs`）。`/api/websearch` 把它放在响应的 `output` 字段里；为什么格式化在
核心这层，见 [docs/10](10-tools-vs-sandbox.md)。

要读全文是**另一件事**，让模型自己挑：

- 静态页面 → `bash` 里的 `curl`
- 要登录态 / JS 渲染 / 点按钮 → `agent browser navigate` 打开那个链接（见 `docs/08`）
- 接口返回 JSON → `bash` 里 `curl … | jq`

（DeepSeek 还有个 `POST /v1/tools/fetch` 能把单页抓成 Markdown，也是同一个 key、5 元/1000
次、QPS 25。暂时没接：全文按需读用 `curl` / `browser` 就够，不必再多一层计费和失败面。
真接的话它和 Search 是一对，放在 `search.rs` 里最合适。）

## 6. 怎么调

不用花 token：

```sh
just phone
  /bash agent search "Claude 最新模型" 3
  /bash agent search "alpine 3.21 release notes"
```

`/api/websearch` 是它的后端（`{query, num_results?, format?}`）。**它也是联网搜索的
唯一入口**（`search` 工具已删，理由见 [docs/10](10-tools-vs-sandbox.md)）。

两种回法：

- 默认 JSON：`{ok, provider, count, output}` —— `output` 就是给模型看的那段文字；
- `format: "text"`：直接回 `text/plain`（一段抬头 `3 条 · DeepSeek` + 正文）。
  **`agent search` 用这个**：JSON 里的换行是 `\n` 转义，模型和界面上看到的就是
  一长串带转义的字符串（“网址后面怎么都有个 `\n`”），而且 CLI 还得靠 `jq`
  才能把它拆开。要原始 JSON 就直接 `agent debug post /api/websearch '{…}'`。

日志：失败会带原始响应片段（key 已打码）；成功的搜索结果不进日志（那是模型上下文的事）。
