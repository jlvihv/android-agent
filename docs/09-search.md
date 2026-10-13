# 联网搜索：Exa，以及为什么它不要钱

> 实测日期：2026-10-12

---

## 1. 两条路

`core/src/search.rs`，思路和 pi-web-access 的 `exa.ts` 一样：

| | 端点 | 凭据 | 限流 |
|---|---|---|---|
| **默认（没 key）** | `https://mcp.exa.ai/mcp?tools=web_search_exa` | **不需要** | 按 IP：2 次/秒、50 次/天 |
| 配了 key | `https://api.exa.ai/search` | `x-api-key` | 按 Exa 账号额度 |

没 key 那条打的是一段**裸的 MCP JSON-RPC**，不做 `initialize` 握手 ——
这个端点无状态，一次 `tools/call` 就够：

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call",
 "params":{"name":"web_search_exa","arguments":{"query":"…","numResults":5}}}
```

响应是 **SSE**（`event: message` + 一行 `data: {…}`）。解析要两种都认：
先扫 `data:` 行，扫不到就整段当 JSON（MCP 规范两种都允许）。

## 2. 为什么不要钱

看 hosted server 的源码（`exa-labs/exa-mcp-server` 的 `api/mcp.ts`）：

- 服务端启动时 `let exaApiKey = process.env.EXA_API_KEY` —— **Exa 自己的 key 在
  它的 Vercel 环境里**，匿名请求就用它去打 Exa API。客户端从头到尾看不到也不需要。
- 免费档限流常量就写在那个文件里：`RATE_LIMIT_QPS=2`、`RATE_LIMIT_DAILY=50`，
  按 IP、**只算 `tools/call`**（`initialize`/`tools/list` 不计数），
  而且只对"没自带 key"的请求生效。

**这是获客漏斗，不是做慈善。** Exa 花钱买"任何 MCP 客户端零配置就能用"这个
默认位，再靠两件事转化：① 免费档只给 `web_search_exa` / `web_fetch_exa`，
好用的 `web_search_advanced_exa`（域名/日期/分类过滤）和 deep researcher
全要自带 key —— 源码里就是 `requiresUserProvidedApiKey: true`；
② 50 次/天一到就弹"去 dashboard 建 key"。

## 3. 手机上必须知道的两个边界

1. **额度按 IP 算**。运营商 CGNAT 下，那 50 次/天 是**同一个出口 IP 上所有人
   共用**的 —— 随时可能撞 429，而且这不是我们的 bug。所以 429 的文案必须
   写出路（等一下再试 / 配 key / 改用 browser 打开搜索引擎），而不是一句
   "搜索失败"。
2. **没有 SLA**。端点和工具名随时可能变。解析失败时错误里要带上原始响应片段，
   这样在手机上也能一眼看出是"接口变了"还是"网络问题"。

## 4. 配 key（免费用完了才需要）

桌面（开发时）：`EXA_API_KEY=exa-… just dev`。

手机：key 存在会话库的 `search` 那一行（和 provider 配置同一个理由：写 JSON 文件
时进程被杀就是半截文件）。设一次：

```sh
# 在电脑上
curl -sS -H "x-agent-token: <token>" -H 'content-type: application/json' \
     -d '{"exa_api_key":"exa-…"}' http://127.0.0.1:18787/api/websearch/config
```

清掉传空字符串。没做界面：这是个"额度用完才需要"的逃生口，先用 curl 设就够。

## 5. 输出为什么这么小气

搜索结果进历史 = 进提示词缓存前缀 = **按钱算**。所以每条片段截 400 字、
整段封顶 4000 字（`core/src/search.rs` 的 `format_hits` —— 搜索不是工具了，
格式化就发生在核心这一层，`/api/websearch` 把它放在响应的 `output` 字段里；
为什么要这样拆，见 [docs/10](10-tools-vs-sandbox.md)）。

要读全文是**另一件事**，让模型自己挑：

- 静态页面 → `bash` 里的 `curl`
- 要登录态 / JS 渲染 / 点按钮 → `agent browser navigate` 打开那个链接（见 `docs/08`）
- 接口返回 JSON → `bash` 里 `curl … | jq`

把十篇全文（几十 KB）塞进历史才是真浪费。

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
- `format: "text"`：直接回 `text/plain`（一段抬头 `3 条 · exa-mcp` + 正文）。
  **`agent search` 用这个**：JSON 里的换行是 `\n` 转义，模型和界面上看到的就是
  一长串带转义的字符串（“网址后面怎么都有个 `\n`”），而且 CLI 还得靠 `jq`
  才能把它拆开。要原始 JSON 就直接 `agent post /api/websearch '{…}'`。

日志：失败会带原始响应片段；成功的搜索结果不进日志（那是模型上下文的事）。
