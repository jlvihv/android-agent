/**
 * 全局状态 + WebSocket 连接。
 *
 * 所有 agent 事件都从 WS 进来，走同一个分发函数；聊天流的 chunk 会进
 * `sse.ts` 的 `applyChunk`。组件只读 store，不自己管连接。
 */

import type {
  Ack,
  AgentEvent,
  ClientMsg,
  DeviceInfo,
  Hello,
} from './types';
import { applyChunk, type Message } from './sse';
import { uid } from './uid';

export type LogItem = { id: number; ts: number; level: string; msg: string };

let logSeq = 0;

/**
 * 尽力开一个新窗口；返回是否成功。
 *
 * 专给「没有用户手势」的自动打开用 —— 失败时**不能**退化成同帧导航，
 * 那会把用户的聊天页导航走（桌面浏览器上尤其明显）。用户点了才走
 * `AgentStore.openExternal`。
 */
function tryOpenNewWindow(url: string): boolean {
  try {
    return !!window.open(url, '_blank', 'noopener,noreferrer');
  } catch {
    return false;
  }
}

class AgentStore {
  connected = $state(false);
  hello = $state<Hello | null>(null);
  device = $state<DeviceInfo | null>(null);
  /** 从 /api/health 读到的 provider 信息；null = 未配置，会走演示流 */
  llm = $state<{
    name: string;
    model: string;
    baseUrl: string;
    /** 思考强度（`minimal`/`low`/`medium`/`high`）；null = 默认档（不发这个参数） */
    reasoningEffort?: string | null;
  } | null>(null);
  /**
   * 当前模型能吃什么输入。决定输入卡片里显示哪些附件入口：
   * `pdf` 为 false 时不显示「文件」（发了也必然 400）。
   * 未配置 provider 时给个保守默认值。
   */
  capabilities = $state<{ vision: boolean; pdf: boolean }>({ vision: true, pdf: false });
  /**
   * 沙箱文件的钥匙：URL 是 `/sandbox/<key>/…`（见 `lib/markdown.ts` 的 `sandboxUrl`）。
   *
   * 为什么在路径里：沙箱里生成的网页跑在 opaque origin，带不上我们的 cookie，
   * 而它的 css/js/图片是相对路径 —— 钥匙放在路径上，子资源自然继承。
   * 从 `/api/health` 拿（那个接口要凭据，别的 App 拿不到）。
   */
  sandboxKey = $state('');
  logs = $state<LogItem[]>([]);
  acks = $state<(Ack & { ts: number })[]>([]);
  /**
   * 沙箱里有人要用浏览器打开网址（`agent-open` → `POST /api/open`）。
   *
   * `opened` = 自动弹窗成功；false 时界面留一条可点的横幅，用户点一下一定开
   * （`gh auth login` 的登录页就是靠这个送到眼前的）。
   */
  openUrl = $state<{ url: string; opened: boolean } | null>(null);
  /** 当前正在流式接收的助手消息 */
  streaming = $state<Message | null>(null);

  #ws: WebSocket | null = null;
  #retry = 0;
  #timer: ReturnType<typeof setTimeout> | undefined;
  #closed = false;

  connect(): void {
    this.#closed = false;
    this.#open();
  }

  disconnect(): void {
    this.#closed = true;
    clearTimeout(this.#timer);
    this.#ws?.close();
    this.#ws = null;
    this.connected = false;
  }

  #open(): void {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${proto}//${location.host}/api/ws`;
    const ws = new WebSocket(url);
    this.#ws = ws;

    ws.onopen = () => {
      this.connected = true;
      this.#retry = 0;
      void this.reloadHealth();
    };

    ws.onclose = () => {
      this.connected = false;
      if (this.#closed) return;
      // 退避重连：手机上 WebView 被切到后台时连接会断，必须能自愈
      const delay = Math.min(500 * 2 ** this.#retry++, 8000);
      this.#timer = setTimeout(() => this.#open(), delay);
    };

    ws.onerror = () => ws.close();

    ws.onmessage = (e) => {
      let ev: AgentEvent;
      try {
        ev = JSON.parse(e.data as string) as AgentEvent;
      } catch {
        this.pushLog('warn', `无法解析的事件: ${String(e.data).slice(0, 120)}`);
        return;
      }
      this.#dispatch(ev);
    };
  }

  /** 重新拉一次 /api/health（改完模型设置后要刷新）*/
  async reloadHealth(): Promise<void> {
    try {
      const r = await fetch('/api/health');
      if (!r.ok) return;
      const h = (await r.json()) as {
        llm?: {
          name: string;
          model: string;
          baseUrl: string;
          reasoningEffort?: string | null;
        } | null;
        capabilities?: { vision: boolean; pdf: boolean } | null;
        sandboxKey?: string;
      };
      this.llm = h.llm ?? null;
      if (h.sandboxKey) this.sandboxKey = h.sandboxKey;
      this.capabilities = h.capabilities ?? { vision: true, pdf: false };
    } catch {
      // health 失败不影响主流程
    }
  }

  #dispatch(ev: AgentEvent): void {
    switch (ev.type) {
      case 'hello':
        this.hello = {
          port: ev.port,
          version: ev.version,
          platform: ev.platform,
          mock: ev.mock,
        };
        break;


      case 'device':
        this.device = { connected: ev.connected, name: ev.name, mock: ev.mock };
        break;

      case 'log':
        this.pushLog(ev.level, ev.msg);
        break;

      case 'ack':
        this.acks = [
          { id: ev.id, ok: ev.ok, detail: ev.detail, ts: Date.now() },
          ...this.acks,
        ].slice(0, 50);
        break;

      case 'agent':
        if (this.streaming) applyChunk(this.streaming, ev.chunk as never);
        break;

      case 'open_url': {
        // 沙箱里有人要开浏览器（agent-open → /api/open）。
        //
        // 先尽力直接开：手机上 WebView 的弹窗策略不一定允许**没有用户手势**
        // 的 window.open，被拦了就留一条可点的横幅（见 App.svelte）。
        const opened = tryOpenNewWindow(ev.url);
        this.openUrl = { url: ev.url, opened };
        break;
      }
    }
  }

  /**
   * 打开一个外部链接（用户点的时候用）。
   *
   * 先试新窗口（桌面浏览器：新标签页）。安卓 WebView 默认
   * `setSupportMultipleWindows(false)`，`window.open` 会直接返回 null 什么也不干，
   * 这时退成**同帧导航** —— MainActivity 的 `shouldOverrideUrlLoading` 会把它
   * 交给系统浏览器，而聊天页自己不动（那个回调返回 true，不真的加载）。
   */
  openExternal(url: string): void {
    if (tryOpenNewWindow(url)) return;
    location.href = url;
  }

  dismissOpenUrl(): void {
    this.openUrl = null;
  }

  pushLog(level: string, msg: string): void {
    this.logs = [{ id: ++logSeq, ts: Date.now(), level, msg }, ...this.logs].slice(0, 300);
  }

  send(msg: ClientMsg): void {
    const ws = this.#ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      this.pushLog('warn', '未连接，消息丢弃');
      return;
    }
    ws.send(JSON.stringify(msg));
  }

  /** 让设备重新读一次 UI 树 */
  /** 直接下发一条指令给设备 */
  /**
   * 开始一条新的助手消息（聊天流用）。
   *
   * 返回值刻意从 `$state` 读回来 —— 这样拿到的是 Svelte 的响应式代理，
   * 后续 `applyChunk` 原地修改才能触发 UI 更新。
   */
  beginStreaming(): Message {
    this.streaming = {
      id: uid(),
      role: 'assistant',
      parts: [],
      steps: 0,
      done: false,
    };
    return this.streaming!;
  }

  clearLogs(): void {
    this.logs = [];
    this.acks = [];
  }
}

export const store = new AgentStore();
