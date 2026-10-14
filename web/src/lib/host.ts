/**
 * 「本机还是局域网里那台电脑」的判断。
 *
 * 为什么单独一个文件：`store`（侧栏底栏那个「现在连的是哪台机器」的小标签）
 * 和 markdown 的沙箱链接都要用它，而 markdown.ts 反过来 import 了 store ——
 * 留在 markdown.ts 里就成了循环引用。
 */

/** 这个主机名是不是本机（App 的 WebView、电脑上的浏览器、Vite 开发服务器） */
export function isLoopbackHost(h: string): boolean {
  return h === '127.0.0.1' || h === 'localhost' || h === '[::1]' || h === '::1';
}
