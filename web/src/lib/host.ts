/**
 * 「页面是不是从本机地址来的」（markdown 的沙箱链接用：`sandbox.localhost` 那一招只在
 * 本机地址上成立）。
 *
 * **别拿它判断「连的是不是手机」**：经中继连电脑时页面也来自 `localhost`（壳的隧道）。
 * 那个问题看 `/api/health` 的 `platform`（见 `store.machine`）。
 */

/** 这个主机名是不是本机（App 的 WebView、电脑上的浏览器、Vite 开发服务器） */
export function isLoopbackHost(h: string): boolean {
  return h === "127.0.0.1" || h === "localhost" || h === "[::1]" || h === "::1";
}
