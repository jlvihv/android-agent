# 12 · 中继：出门在外连家里的电脑

局域网里，手机靠 mDNS 找到电脑、输一次配对码就能连（docs/11）。出了门这条路就断了：
手机找不到电脑，电脑在家里的 NAT 后面也连不进去。中继补上这一段 —— 一台有公网 IP 的
服务器（**不需要域名**），两头都主动连它，它把两边接起来。

```text
家里电脑的核心 ──HOST（主动连出去，一直挂着）──▶ 中继 ◀──DIAL── 外面的手机
               ◀──CONNECT <票>────────────────
               ──ACCEPT <票>（为这次新开一条）─▶  两条接上，之后只搬字节
```

## 用起来是什么样

1. 服务器上装中继：`just relay-deploy root@<服务器IP>`（交叉编译 + scp + systemd，见下）。
   最后会打印两行 `AGENT_RELAY=…` / `AGENT_RELAY_KEY=…`。
2. 家里电脑把这两行写进仓库根目录的 `.env.local`，重启 `just dev-host`。终端里会说
   「出门在外：经中继 …」，日志里有「已连上中继」。
3. 手机**在家里的 Wi-Fi 里**配对一次（或者已经配过的，在家连一次）—— 手机这时记下
   这台电脑的证书指纹和中继地址。
4. 出门以后，「选择机器」面板里那台电脑变成「经服务器连」，点一下就连上。
   界面、命令、文件和在家一模一样。

面板里同一台电脑**只出现一次**：局域网里找得到就只显示直连那条；找不到（出门了）才显示
经服务器那条。连着的局域网直连断了（拿着手机出了门），壳会**自动**改走中继，不用回到面板。
回到家，面板里直连那条又出现了，点一下就换回直连。

## 安全

配对得到的 token 等于家里这台电脑的 shell，**绝不能明文过公网**。所以：

- **端到端 TLS，中继看不到内容。** 家里的核心第一次用时生成一张自签证书（存在
  `~/.local/share/android-agent/remote/`，之后一直是这一张）。手机配对时记下它的
  SHA-256 指纹；经中继连过来时**只认这一张**（`tunnel.rs` 的 `Pinned`：比指纹，
  签名照常验 —— 证书是公开的，验签保证对面真有那把私钥）。中继、或者路上的任何人
  都冒充不了这台电脑。服务器被人拿下也拿不到会话内容和凭据。
- **中继自己的门：共享密钥 + 挑战-应答。** 中继第一次启动时生成一把 32 字节的密钥。
  握手时中继先给一个随机数，对面回 `HMAC-SHA256(密钥, 角色|id|随机数)`：密钥不过网，
  抓到的握手重放不了。没密钥的人既不能冒充家里那台占位，也不能拿服务器当免费中转。
  密钥在配对时（和 `/api/remote`）交给手机 —— 手机本来就拿着 shell 的 token，
  再多一把中继的钥匙不改变什么。
- **经中继来的请求一律按外人对待。** 家里的核心把它们的对端地址记成 `0.0.0.0:0`
  （`remote::RELAY_PEER`），所以「电脑自己来的不要凭据」（`trust_loopback`）对它们
  永远不成立 —— 哪怕中继就跑在这台电脑上。`/api/pair` 也不接（403，见 `http::ViaRelay`）：
  配对码只该在局域网里输，不然公网上有中继密钥的人能来猜（猜错五次还会把配对整个停掉）。
- **家里不开端口。** 电脑主动连出去；中继只听一个端口。

## 形状

| 部分 | 在哪 | 做什么 |
|---|---|---|
| 中继服务器 | `relay/`（独立小 crate，二进制 `agent-relay`） | 接线 + 搬字节；状态只有「谁在线」「哪些拨号在等」 |
| 握手协议 | `relay/src/lib.rs`（`connect` / `run_host`） | 两头的核心共用这一份 |
| 家里这头 | `core/src/remote.rs` | 证书、挂在中继上、每条连接 TLS 握手后把**同一份路由**挂上去（hyper，带 WebSocket 升级） |
| 手机这头 | `core/src/tunnel.rs` + `ffi::AgentCore::start_tunnel` | 本机 `127.0.0.1:<端口>` 起代理，每条连接拨号 + TLS（只认那个指纹） |
| 壳 | `MainActivity`（`startRelay` / `relayMachines` / `refreshRelayInfo`） | 存每台电脑的中继信息、面板去重、直连失败自动改走中继 |

几个选择：

- **一条用户连接对应一对 TCP**（手机那条 + 电脑为它新开的那条），不在一条连接上做多路复用。
  中继因此没有自己的帧格式，代码很小；代价是每条新连接多一次往返（WebView 有 keep-alive，
  感觉不到）。
- **手机这头是本机代理，不是让 WebView 直连。** WebView 不认「只信这一张证书」；而且页面来自
  `localhost`，算安全上下文（局域网直连 `192.168.x.x` 不算）。
- **用 `localhost` 而不是 `127.0.0.1`**：cookie 按主机名分，手机自己的核心在 `127.0.0.1` 上，
  两边的 `agent_token` 不会互相覆盖。
- **中继信息存在手机上、每次在家连上都对一遍**（`GET /api/remote`）：中继是配对之后才配的、
  或者换了服务器，都不用重新配对。
- **中继只用纯 Rust 依赖**（tokio + hmac + sha2，没有 ring / C 代码），所以在 macOS 上用
  `rust-lld` 就能直接交叉编译出 Linux x86_64 的静态二进制（~0.7MB），服务器上什么都不用装。
- **连不上时说人话。** 拨号失败 / 指纹不对，隧道回一页 502 写明原因（先把请求读掉再回 ——
  带着没读的数据关连接，系统会发 RST，浏览器只看到「连接被重置」）。

## 部署

```sh
just relay-build                 # → relay/target/x86_64-unknown-linux-musl/release/agent-relay
just relay-deploy root@1.2.3.4   # 或 RELAY_PORT=9000 scripts/deploy-relay.sh ubuntu@1.2.3.4
```

`deploy-relay.sh` 把二进制装到 `/usr/local/bin/agent-relay`，写一个 systemd 服务
（`DynamicUser`，不用 root 跑；密钥在 `StateDirectory` 里，`/var/lib/agent-relay/key`），
开启并启动，装了 ufw 就顺手放行端口，最后打印家里要设的两行。再跑一次就是升级（密钥不变）。
**云服务器的安全组**得自己去控制台放行 TCP 8790。

不用 systemd 的话，直接跑也行：

```sh
AGENT_RELAY_LISTEN=0.0.0.0:8790 AGENT_RELAY_KEY_FILE=/var/lib/agent-relay/key ./agent-relay
```

## 验证（2026-10-14，本机）

自动测试：`relay` 5 条（拨号接通双向搬字节、对方不在线、密钥不对被拒、mac 绑定随机数和角色、
参数校验）；`core` 的 `remote::tests::a_phone_reaches_the_home_core_through_the_relay` 在一个
进程里跑真中继 + 家里那头（真路由）+ 手机那头的隧道：带凭据 200、不带 401（中继也在本机，
照样要凭据）、`/api/pair` 403、`/api/remote` 带指纹、指纹不对 502、对方不在线 502。

手动（真二进制：本机中继 + 一个独立数据目录的临时核心 + `cargo run --example tunnel`
扮演手机）：首页 200、`/api/health` 不带凭据 401、`/auth` 种 cookie 后 200、经中继配对 403、
`/api/ws` 升级 101、`/api/turns/stream` 首帧正常；把中继杀掉再起，家里那头 2 秒内自己接回来，
中间那段隧道回的是说明页。

```sh
# 扮演出门在外的手机（参数从家里那台的 /api/remote 拿：电脑上 curl 127.0.0.1:8787/api/remote）
cargo run --manifest-path core/Cargo.toml --example tunnel -- <中继IP:端口> <密钥> <实例id> <指纹>
```
