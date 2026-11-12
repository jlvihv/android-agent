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
 *   - 换会话：`pushState({ route: true }, "", "/s/<id>")` —— 地址**变**
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
  // `state` 写成 `{ route: true }`（不是 `null`）：popstate 的两个监听者靠它
  // 认领各自的条目 —— 覆盖层看 `overlay`，这里看 `route`（见 `onRouteChange`）。
  // 用 `null` 的话「谁也认领不到」，两边都得靠猜。
  if (replace) history.replaceState({ route: true }, "", want);
  else history.pushState({ route: true }, "", want);
}

/**
 * 离开某条会话（新建 / 删除之后）：把地址收回根路径，同样不重复压历史。
 */
export function clearUrl(): void {
  if (location.pathname === "/") return;
  // 换地址时要**保留我们自己那个标记**（`{route:true}`），别把覆盖层的 state
  // 原样搬过来 —— 那样这条路由条目会被覆盖层认领走。
  history.replaceState({ route: true }, "", "/");
}

/**
 * 给**当前**这条 history 条目打上我们的标记。
 *
 * 必须在启动时调一次：首屏那条是浏览器自己压的（`state` 是 `null`），
 * 于是「从第一条会话后退回首屏」时 `route` 判据认不出它是「落到了一条路由条目上」
 * —— 界面不切换，而地址已经变了，两边对不上。
 *
 * 用 `replaceState`（不是 push）：不新增历史，只改这一条的 `state`。
 *
 * 为什么不干脆让判据「`state` 是 null 也算路由」：那样覆盖层弹栈（它的条目一定
 * 带 `{overlay:n}`）之外的**任何** null 条目都会被路由认领，包括将来别人压的。
 * 显式标记更硬：谁压的谁负责标记。
 */
export function markCurrentEntry(): void {
  const st = history.state as { overlay?: number; route?: boolean } | null;
  // 已经是覆盖层那条就别动（那一条归 overlay 管）
  if (st?.overlay !== undefined) return;
  history.replaceState({ ...(st ?? {}), route: true }, "");
}

/**
 * 订阅「用户在地址栏里换了会话」（后退 / 前进）。
 *
 * 返回退订函数。
 *
 * **按 `history.state` 认领自己的条目**，不靠「地址变了没变」去猜：
 *
 *   - 我们压的：`{ route: true }`（见 `syncUrl` / `clearUrl`）
 *   - 覆盖层压的：`{ overlay: n }`（见 `lib/overlay.ts`）
 *
 * 两个监听者各看各的字段，互不误伤。以前这里是「一收到 popstate 就按当前地址
 * 算一遍会话 id」，而覆盖层弹栈时地址不变 —— 于是它也算出一个「等于当前会话」
 * 的 id，被 `id === sessionId` 那道早退挡住。能跑，但挡子弹的是别人的判断。
 */
export function onRouteChange(fn: (id: string | null) => void): () => void {
  const handler = (e: PopStateEvent) => {
    const st = e.state as { route?: boolean; overlay?: number } | null;
    // 判据：**带 `route` 且不带 `overlay`**（见 `lib/overlay.ts` 同名判断）
    if (!st?.route || st.overlay !== undefined) return;
    fn(sessionFromPath());
  };
  addEventListener("popstate", handler);
  return () => removeEventListener("popstate", handler);
}
