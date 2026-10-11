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
  Command,
  DeviceInfo,
  Hello,
  UiSnapshot,
} from './types';
import { applyChunk, type Message } from './sse';
import { uid } from './uid';

export type LogItem = { id: number; ts: number; level: string; msg: string };

let logSeq = 0;

class AgentStore {
  connected = $state(false);
  hello = $state<Hello | null>(null);
  device = $state<DeviceInfo | null>(null);
  /** 从 /api/health 读到的 provider 信息；null = 未配置，会走演示流 */
  llm = $state<{ name: string; model: string; baseUrl: string } | null>(null);
  snapshot = $state<UiSnapshot | null>(null);
  logs = $state<LogItem[]>([]);
  acks = $state<(Ack & { ts: number })[]>([]);
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
        llm?: { name: string; model: string; baseUrl: string } | null;
      };
      this.llm = h.llm ?? null;
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

      case 'snapshot':
        this.snapshot = ev.data;
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
    }
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
  refresh(): void {
    this.send({ type: 'refresh' });
  }

  /** 直接下发一条指令给设备 */
  command(cmd: Command): void {
    this.send({ type: 'command', cmd });
  }

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
