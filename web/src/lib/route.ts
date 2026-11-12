/**
 * 会话在地址栏里怎么表示。
 *
 * **这就是「现在看的是哪条会话」唯一的前端真相**（以前它只活在 `App` 的
 * `$state` 里，地址栏永远是 `/`：刷新回到服务端认为的 active、分享不出去、
 * 前进后退也不会换会话）。
 *
 * 为什么是**路径** `/s/<id>` 而不是 hash `#/s/<id>`：
 *   - 服务端已经能把任意「看起来像页面」的路径回成 `index.html`（`assets.rs`
 *     的 SPA fallback），刷新和分享都是现成的，不用额外配置；
 *   - 手机壳 / `sandbox.localhost` / 各个中继加载的就是 `http://host:port/`，
 *     hash 和 path 两种它都无所谓，但 path 更像一个真地址；
 *   - `#` 在有些环境里会被吞掉（老 WebView、某些代理），path 没有这个风险。
 *
 * 为什么不用 History API 的 `state` 存 id：那样地址栏看不出是哪条会话，
 * 分享出去还是 `/`，等于没做。id 必须在 **URL 里**。
 *
 * 覆盖层的返回键（`lib/overlay.ts`）也用 history，两者的约定是分开的：
 *   - 覆盖层：`pushState({ overlay: n })` —— 地址**不变**
 *   - 换会话：`pushState(null, "", "/s/<id>")` —— 地址**变**
 * 后退时先弹覆盖层、再换会话，顺序天然正确（覆盖层是后压进去的）。
 */

/** 一条会话的地址 */
export const sessionPath = (id: string): string =>
  id ? `/s/${encodeURIComponent(id)}` : "/";

/**
 * 从当前地址解出会话 id。不是 `/s/<id>` 就返回 `null`（= 地址里没指定，
 * 由调用方决定回哪条）。
 *
 * 只认完整路径段：`/s/abc/extra` 也算 `abc`（将来加子路由不至于立刻坏），
 * 但 `/sessions/abc` 不算。
 */
export function sessionFromPath(pathname = location.pathname): string | null {
  const m = /^\/s\/([^/]+)/.exec(pathname);
  if (!m) return null;
  try {
    return decodeURIComponent(m[1]);
  } catch {
    return m[1]; // 坏的百分号编码：原样给出去，让服务端判它不存在
  }
}

/**
 * 把当前会话写进地址栏。
 *
 * `replace` 用在「补上地址」的场合（首屏进来时 URL 还是 `/`，定位到某条会话后
 * 安静地补上，不该在历史里留一格）；换会话时用 `push`，这样后退能回上一条。
 *
 * **地址没变就什么都不做**：`pushState` 一条相同的地址会往历史里塞重复项，
 * 用户按后退看着像没反应。
 */
export function syncUrl(id: string, replace = false): void {
  const want = sessionPath(id);
  if (location.pathname === want) return;
  if (replace) history.replaceState(history.state, "", want);
  else history.pushState(null, "", want);
}

/**
 * 离开某条会话（新建 / 删除之后）：把地址收回根路径，同样不重复压历史。
 */
export function clearUrl(): void {
  if (location.pathname === "/") return;
  history.replaceState(history.state, "", "/");
}

/**
 * 订阅「用户在地址栏里换了会话」（后退 / 前进）。
 *
 * 返回退订函数。**只处理换会话那一类 popstate**：覆盖层的 popstate 由
 * `lib/overlay.ts` 自己接（它判断的是 `skip` 和栈，不看地址）。
 */
export function onRouteChange(fn: (id: string | null) => void): () => void {
  const handler = () => fn(sessionFromPath());
  addEventListener("popstate", handler);
  return () => removeEventListener("popstate", handler);
}
