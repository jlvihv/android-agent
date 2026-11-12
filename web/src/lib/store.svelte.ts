/**
 * 全局状态 + WebSocket 连接。
 *
 * WS 上来的是设备 / 日志 / 回执这些事件，走同一个分发函数；聊天流走
 * `/api/chat` 的 SSE（chunk 进 `sse.ts` 的 `applyChunk`）。组件只读 store，
 * 不自己管连接。
 *
 * 后两种（`log` / `ack`）页面不渲染，只转到 devtools 的 console —— 它们是给
 * 调试用的（服务端那侧同时进 tracing）。**别在这里攒数组**：攒了没人看，
 * 还白占内存（以前就是这么烂掉的）。
 */

import type { AgentEvent } from "./types";
import type { Message } from "./sse";
import { uid } from "./uid";

/** WebSocket 退避重连：起始间隔（ms），每次翻倍，封顶 `RETRY_MAX_MS` */
const RETRY_BASE_MS = 500;
const RETRY_MAX_MS = 8000;
/** 解析不了的事件，日志里只留前这么多字符 */
const LOG_SNIPPET = 120;

class AgentStore {
  connected = $state(false);
  /**
   * 这个核心跑在哪台机器上（`/api/health` 的 `computer`）。
   *
   * 手机上是 `null` —— 安卓进程里的主机名就是 `localhost`，没信息量
   * （core 那边干脆不发，见 `http.rs` 的 `computer_name`）。电脑上是它的名字，
   * 和 mDNS 广播 / 配对码那两处是同一个（`desktop::computer_name`）。
   */
  computer = $state<string | null>(null);
  /** 跑这个核心的系统（`/api/health` 的 `platform`）：`android` = 手机自己的核心 */
  platform = $state("");
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
    const os = this.platform;
    if (os === "android") return { name: "本机", phone: true };
    if (os) return { name: this.computer ?? location.hostname, phone: false };
    // 只认 127.0.0.1：手机自己的核心就在那儿。经中继连电脑时页面来自 `localhost`
    // （壳的隧道，见 docs/12）—— 那是另一台机器，说「本机」就错了
    return location.hostname === "127.0.0.1"
      ? { name: "本机", phone: true }
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
  capabilities = $state<{ vision: boolean; pdf: boolean }>({
    vision: true,
    pdf: false,
  });
  /**
   * 沙箱文件的钥匙：URL 是 `/sandbox/<key>/…`（见 `lib/markdown.ts` 的 `sandboxUrl`）。
   *
   * 为什么在路径里：沙箱里生成的网页跑在 opaque origin，带不上我们的 cookie，
   * 而它的 css/js/图片是相对路径 —— 钥匙放在路径上，子资源自然继承。
   * 从 `/api/health` 拿（那个接口要凭据，别的 App 拿不到）。
   */
  sandboxKey = $state("");
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
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
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
        console.warn(
          `[ws] 无法解析的事件: ${String(e.data).slice(0, LOG_SNIPPET)}`,
        );
        return;
      }
      this.#dispatch(ev);
    };
  }

  /** 重新拉一次 /api/health（改完模型设置后要刷新）*/
  async reloadHealth(): Promise<void> {
    try {
      const r = await fetch("/api/health");
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
      // `hello` / `device` 页面不显示（平台看 `/api/health` 的 `platform`，
      // 设备连接状态只在手机壳的通知栏里用）。以前把它们攒进 `$state` 就再没有
      // 一处读过 —— 两个白占内存的字段，删了。
      case "hello":
      case "device":
        break;

      case "log":
        console.debug(`[core ${ev.level}]`, ev.msg);
        break;

      case "ack":
        console.debug(`[core ack ${ev.id}]`, ev.ok, ev.detail);
        break;

      case "turn_active":
        this.turnActive = ev.active;
        break;

      // 逐条状态（`turn`）只喂通知栏那一行，页面不吃：正文和消息列表有自己的路
      // （正在跑的那条流 / 轮询）。这里置个位就够。
      case "turn":
        this.turnActive = true;
        break;
    }
  }

  /**
   * 一条空的助手消息（聊天流从这里开始写）。
   *
   * **返回的必须是 `$state` 代理**，不能是普通对象 —— 这一条不是风格问题：
   * `applyChunk` 是**原地**往 `parts` 里追加 delta 的，而流式期间没有任何东西会
   * 重新给 `messages[i]` 赋值。所以「改的是不是代理」直接决定界面动不动：
   *
   *   - 返回普通对象 → push 进 `messages` 之后，流写的是**原始对象**，
   *     Svelte 的代理感知不到（实测：effect 不重跑），界面上只有用户那条气泡，
   *     模型的回复永远不出现（踩过，而且看起来就像「SSE 没推送」）。
   *   - 先写进一个 `$state`、再**读回来** → 拿到代理，写它才发信号。
   *
   * 以前这里还额外存了一份 `this.streaming`（全项目没有一处读，副作用是 store
   * 永久抓着最后那条消息，含几十 KB 工具输出）。**留着它的作用恰恰是造出代理**；
   * 我一度把它删成「直接返回字面量」，就把流式推送整个弄哑了。现在用一个只在本
   * 方法里活着的临时 `$state` 完成同一件事，既不留常驻引用，也还是代理。
   *
   * 回归测试见 `tests/streaming.test.ts`（它盯的就是「写进去能不能被看见」）。
   */
  beginStreaming(): Message {
    const box = $state<{ msg: Message }>({
      msg: { id: uid(), role: "assistant", parts: [], done: false },
    });
    return box.msg;
  }
}

export const store = new AgentStore();
