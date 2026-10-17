/**
 * 全局状态 + WebSocket 连接。
 *
 * WS 上来的是设备 / 日志 / 回执这些事件，走同一个分发函数；聊天流走
 * `/api/chat` 的 SSE（chunk 进 `sse.ts` 的 `applyChunk`）。组件只读 store，
 * 不自己管连接。
 */

import type {
  Ack,
  AgentEvent,
  ClientMsg,
  DeviceInfo,
  Hello,
} from './types';
import type { Message } from './sse';
import { uid } from './uid';

export type LogItem = { id: number; ts: number; level: string; msg: string };

let logSeq = 0;

/** WebSocket 退避重连：起始间隔（ms），每次翻倍，封顶 `RETRY_MAX_MS` */
const RETRY_BASE_MS = 500;
const RETRY_MAX_MS = 8000;
/** 解析不了的事件，日志里只留前这么多字符 */
const LOG_SNIPPET = 120;
/** 日志 / 指令回执各自最多留多少条（进抽屉的面板看的，不是审计） */
const MAX_LOGS = 300;
const MAX_ACKS = 50;

class AgentStore {
  connected = $state(false);
  hello = $state<Hello | null>(null);
  device = $state<DeviceInfo | null>(null);
  /**
   * 这个核心跑在哪台机器上（`/api/health` 的 `computer`）。
   *
   * 手机上是 `null` —— 安卓进程里的主机名就是 `localhost`，没信息量
   * （core 那边干脆不发，见 `http.rs` 的 `computer_name`）。电脑上是它的名字，
   * 和 mDNS 广播 / 配对码那两处是同一个（`desktop::computer_name`）。
   */
  computer = $state<string | null>(null);
  /** 跑这个核心的系统（`/api/health` 的 `platform`）：`android` = 手机自己的核心 */
  platform = $state('');
  /**
   * 给这个页面的核心的实例 id（`/api/health` 的 `id`）。经隧道时是那台电脑的 ——
   * 「选择机器」靠它勾「现在这页是哪台」（见 `lib/machines.svelte.ts`）。
   */
  instance = $state<string | null>(null);

  /**
   * 侧栏底栏那个小标签上写什么 —— 现在连的是哪台机器。
   *
   *   - 手机自己的核心 →「本机」
   *   - 电脑上的核心 → 那台电脑的名字（`/api/health` 的 `computer`）
   *
   * 两个来源都还没到（页面刚起来那一两帧）就退到地址上：`127.0.0.1` 是手机
   * 自己的核心，别的都当成电脑，先拿地址撑着 —— 总比空着强，而且**不会说错**。
   */
  machine = $derived.by((): { name: string; phone: boolean } => {
    const os = this.platform || this.hello?.platform || '';
    if (os === 'android') return { name: '本机', phone: true };
    if (os) return { name: this.computer ?? location.hostname, phone: false };
    // 只认 127.0.0.1：手机自己的核心就在那儿。经中继连电脑时页面来自 `localhost`
    // （壳的隧道，见 docs/12）—— 那是另一台机器，说「本机」就错了
    return location.hostname === '127.0.0.1'
      ? { name: '本机', phone: true }
      : { name: location.hostname, phone: false };
  });
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
  /** 当前正在流式接收的助手消息 */
  streaming = $state<Message | null>(null);
  /**
   * **有没有回合在跑**（总线上的 `turn_active`；全局的，不分会话）。
   *
   * 页面自己发起的回合它自己清楚（`App` 的 `busy`），可别人发起的它一无所知 ——
   * 手机上按的、另一台电脑上那个页面按的、排队消息接着开的那个回合，都是别人在动。
   * 所以这个位只当**信号**用：一变，`App` 就去把会话对一次账（`loadMessages`）。
   *
   * 不带会话 id（核心那边也只有一个全局的「有没有」）——对账时 `?session` 会
   * 如实回答「**这个**会话在不在跑」，不是这个会话在跑就对完收工，不轮询。
   */
  turnActive = $state(false);

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
      const delay = Math.min(RETRY_BASE_MS * 2 ** this.#retry++, RETRY_MAX_MS);
      this.#timer = setTimeout(() => this.#open(), delay);
    };

    ws.onerror = () => ws.close();

    ws.onmessage = (e) => {
      let ev: AgentEvent;
      try {
        ev = JSON.parse(e.data as string) as AgentEvent;
      } catch {
        this.pushLog('warn', `无法解析的事件: ${String(e.data).slice(0, LOG_SNIPPET)}`);
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
        platform?: string;
        computer?: string | null;
        id?: string;
      };
      this.llm = h.llm ?? null;
      if (h.sandboxKey) this.sandboxKey = h.sandboxKey;
      this.capabilities = h.capabilities ?? { vision: true, pdf: false };
      if (h.platform) this.platform = h.platform;
      this.computer = h.computer ?? null;
      if (h.id) this.instance = h.id;
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
        ].slice(0, MAX_ACKS);
        break;

      case 'turn_active':
        this.turnActive = ev.active;
        break;

      // 逐条状态（`turn`）只喂通知栏那一行，页面不吃：正文和消息列表有自己的路
      // （正在跑的那条流 / 轮询）。这里置个位就够。
      case 'turn':
        this.turnActive = true;
        break;
    }
  }

  pushLog(level: string, msg: string): void {
    this.logs = [{ id: ++logSeq, ts: Date.now(), level, msg }, ...this.logs].slice(0, MAX_LOGS);
  }

  send(msg: ClientMsg): void {
    const ws = this.#ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      this.pushLog('warn', '未连接，消息丢弃');
      return;
    }
    ws.send(JSON.stringify(msg));
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
