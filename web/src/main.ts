import { mount } from "svelte";
import "./app.css";
import App from "./App.svelte";
import { store } from "./lib/store.svelte.ts";

/**
 * 输入法。
 *
 * 默认行为：WebView 不缩小，浏览器把整个可视区往上平移、让输入框露出来
 * —— 头部也跟着被顶出屏幕（平移了 295px）。Android 的 adjustResize
 * 在 edge-to-edge 模式下不生效，所以不能指望 WebView 变矮。
 *
 * 改成：键盘浮在页面上（overlaysContent），页面不动；键盘高度从
 * visualViewport 算出来写进 `--kb`，只有输入卡片和面板跟着往上抬。
 * WebView 里 VirtualKeyboard 的 geometrychange 事件不触发，
 * 所以不用 env(keyboard-inset-height)。
 */
const vk = (
  navigator as Navigator & { virtualKeyboard?: { overlaysContent: boolean } }
).virtualKeyboard;
if (vk) vk.overlaysContent = true;

const vv = window.visualViewport;
if (vv) {
  const sync = () => {
    const kb = Math.max(0, Math.round(innerHeight - vv.height - vv.offsetTop));
    document.documentElement.style.setProperty("--kb", `${kb}px`);
  };
  vv.addEventListener("resize", sync);
  vv.addEventListener("scroll", sync);
  sync();
}

// 立刻连 WS，别等组件挂载 —— 手机 WebView 从后台回来时能早点重连
store.connect();

const app = mount(App, { target: document.getElementById("app")! });

export default app;
