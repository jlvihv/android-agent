import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import tailwindcss from "@tailwindcss/vite";

// 不引 @types/node 就为了这一行 —— 用一个最小声明代替
declare const process: { env: Record<string, string | undefined> };

// 生产环境：前端由 Rust 的 axum 从 http://127.0.0.1:<port> 同源提供，
// 所以代码里一律用相对路径（/api/...），不需要任何 base 配置。
//
// 开发环境：Vite 跑在 5173，把 /api 代理到 Rust 的 8787。
// 这样在桌面浏览器里开发时，跟手机上 WebView 看到的是同一套相对路径。
const CORE = process.env.AGENT_CORE ?? "http://127.0.0.1:8787";
// 核心的 /api 要凭据（手机上别的 App 也能连 127.0.0.1）。对着真机开发时
// 由代理替浏览器带上（`scripts/phone-token.sh` 拿）；桌面核心没设就不用。
const TOKEN = process.env.AGENT_TOKEN ?? "";
const AUTH = TOKEN ? { "x-agent-token": TOKEN } : undefined;

export default defineConfig({
  plugins: [tailwindcss(), svelte()],
  server: {
    port: 5173,
    host: true,
    proxy: {
      "/api": { target: CORE, ws: true, changeOrigin: false, headers: AUTH },
      // 沙箱文件（查看器里的网页/图片、markdown 里的 sandbox: 链接）也在核心那边。
      // 只代理 /api 的话这些请求会落回 Vite 自己 —— 它给不存在的路径吐 index.html，
      // 于是查看器里显示的是 App 自己的界面（踩过：探测底色全部探成了深色）。
      "/sandbox": { target: CORE, changeOrigin: false },
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2022",
    // 手机上 WebView 走本地回环，不需要分包
    chunkSizeWarningLimit: 2048,
  },
});
