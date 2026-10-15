# MCP 和 Skill：都不进工具表

> 2026-10-16

两样东西都是「给 agent 加能力」，但都**不开成工具**：工具表是缓存前缀的一部分，
增删一个就让所有会话的缓存整段作废；而且 MCP 的工具 schema 动辄上万 token，用不用
都每轮带着。思路和浏览器、搜索一样（见 [docs/10](10-tools-vs-sandbox.md)）：能力从
沙箱这一个门进，提示词里只放「有什么」，细节按需去看。

## MCP：`agent mcp`，只接远程服务

```sh
agent mcp list [服务]                    # 已接的服务和工具，一行一个
agent mcp <服务> --help                  # 同上（只看这一个服务）
agent mcp <服务> <工具> --help           # 这个工具的说明和参数 schema
agent mcp <服务> <工具> ['<json参数>']    # 调用
```

- **只接远程（Streamable HTTP）**，不在本地起服务进程（stdio 的 `npx` / `uvx`）。配置
  只有名字、URL、请求头（`mcp_servers` 表），从结构上就填不进一条本地命令。
- **核心是客户端**（`core/src/mcp.rs`）：`initialize` 握手、记 `Mcp-Session-Id`、
  会话过期（404）自动重握一次、`tools/list` 翻页；响应是 SSE 还是 JSON 都认。
  连接层失败（请求没发出去）重试一次，服务端回了响应就不重试（可能已经执行过）。
- **凭据不进沙箱**：token 存在核心的库里，核心带着去请求。`agent` 命令拒绝碰
  `/api/mcp/servers`（接哪些服务由人在 设置 → MCP 服务 里定 —— 网页里的一段话不该能
  让 agent 把调用转给别人的服务）。
- **输出是纯文本**，出错以 `错误：` 开头（CLI 据此非 0 退出）。工具回的图片落盘、给
  路径，模型用 `read` 看：手机上落在**这个会话自己的目录**下的 `mcp/`（一个会话的东西
  都在一处），电脑宿主模式没有会话目录（cwd 是用户的项目，不往里写），落在临时目录、
  由启动清扫回收（`agent-out-` 前缀）。
- **提示词里只有一句**：接了哪几个服务（`mcp::prompt_line`）。没接就没有这一句。

## Skill：一个目录，提示词里放索引

一个 skill = 一个目录：`SKILL.md`（开头的 frontmatter 写 `name` / `description`，正文写
做法）+ 它要用的脚本和资料。

- **不加命令**：用 skill 就是 `read` 它的 `SKILL.md`、用 bash 跑里面的脚本，现有的门
  就够了。
- **索引进系统提示词**（名字 + 一句描述 + 路径，`skills::index`）：skill 靠「描述被
  看见」来触发；只给一个「去查有哪些」的命令，模型不知道什么时候该查。正文不进提示词。
- **放在哪**（`LinuxSandbox::skills_dir`）：手机上 `/workspace/skills`（挂了存储就是
  内部存储的 Agent 文件夹，用户在文件管理器里看得到、能自己放）；电脑上
  `~/.local/share/android-agent/skills`（不跟着项目走）。装一个就是放一个目录进去，
  让 agent 自己 `git clone` 也行。设置页里有只读列表。

## 动态的那段提示词

接了哪些服务、装了哪些 skill 都会变，所以拼在冻结的那份提示词**后面**
（`AppState::system_prompt`）：

- 不变时，每个会话、每一轮拿到的都是同一串字节（按名字排序），缓存照常命中；
- 变了，`ensure_system_prompt` 会把新版本追加进老会话，下一轮全价一次。所以这段要
  **短**：MCP 只列服务名，skill 只列一句描述（截到 300 字，最多 50 个）。
