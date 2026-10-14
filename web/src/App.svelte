<script lang="ts">
  /**
   * 外壳。结构对齐 Claude 移动端：
   *   header（菜单 + 状态）/ main（空态标题 或 消息列表）/ 输入卡片
   *   抽屉 = Claude 侧边栏（导航项 + 最近对话 + 新建会话）
   *
   * 没有魔法数字：尺寸走 Tailwind 刻度，顶部/底部由安全区给。
   */
  import { untrack } from 'svelte';
  import { store } from './lib/store.svelte.ts';
  import {
    consume,
    isQueued,
    prepend,
    streamFrom,
    onQueuedDelivered,
    onUsage,
    cachedTokens,
    type Message,
    type Part,
    type UiChunk,
    type Upload,
    type Usage,
    type UsageTotals,
    type SessionUsage,
  } from './lib/sse';
  import { uid } from './lib/uid';
  import Icon from './components/Icon.svelte';
  import Composer from './components/Composer.svelte';
  import MessageList from './components/MessageList.svelte';
  import Drawer, { type Session } from './components/Drawer.svelte';
  import MachineSheet from './components/MachineSheet.svelte';
  import ModelSheet from './components/ModelSheet.svelte';
  import Settings from './components/Settings.svelte';
  import Viewer from './components/Viewer.svelte';
  import { viewer } from './lib/viewer.svelte.ts';
  import { models } from './lib/models.svelte.ts';
  import { agentHasBrowser, canSwitchCore, openAgentBrowser } from './lib/overlay';
  import { wide } from './lib/wide.svelte.ts';

  let input = $state('');
  /** 当前看着的这个会话，本地正连着它的流。 */
  let busy = $state(false);
  /** 服务端说当前会话还在跑。App 切到后台、或者从别的会话切回来时，
   *  本地没连着流，靠它判断"还在进行中"（然后轮询）。 */
  let serverBusy = $state(false);
  let messages = $state<Message[]>([]);
  let sessions = $state<Session[]>([]);

  /**
   * 排队等着交给模型的消息 —— 它是**服务端**那份队列的本地镜子。
   *
   * 队列本身在核心（`AppState::queued`）：`POST /api/chat` 碰上这个会话正跑着
   * 就把它排进去，正在跑的那个回合**一空下来**（步边界）就把它交给模型 —— 不用
   * 等整个回合结束。为什么不在前端排：前端那个队列活在 WebView 里，App 切到
   * 后台、WebView 被冻住或回收，这条消息就永远发不出去了（见 docs/05 的「排队」）。
   *
   * 这一份只负责**画**（虚线气泡 + 左边那个 ×），三个来源：
   *
   * 1. `send` 里刚发出去、还没听到回话的那条（`pending: true`）—— 服务端说
   *    「排队了」就坐实，说「开跑了」就翻进正文（见 `deliver`）。
   * 2. 服务端把排队的消息交给模型时推的 `data-queued-delivered`：从这里挪进正文。
   * 3. `loadMessages` 每次按 `/api/session` 的 `queued` **整份重来** —— App 被
   *    回收过 / 换了设备 / 界面刚刷新时，那几条就是从这儿回来的。
   *
   * 每条记着自己属于哪个会话：回合在后台跑着时用户可能已经切走了。
   */
  type Queued = { id: string; sid: string; parts: Part[]; pending?: boolean };
  let queue = $state<Queued[]>([]);

  /**
   * 本会话的 token 用量（输入框里那个上下文圆环）。
   *
   * 两个来源：流里的 `data-usage`（每步结束一条，实时）+ `/api/session` 的
   * `usage`（刷新 / 切会话回来时的基线）。累计值以服务端为准 —— 流里那些
   * 增量只是两次拉取之间的临时账，下一次响应整份换掉（两边数的是同一批
   * 步骤，对得上）。
   */
  let usage = $state<SessionUsage | null>(null);

  // 圆环的分母：模型窗口。清单是启动时就拉好的（见下面的 models.load）
  let ctxWindow = $derived(models.contextWindow(store.llm?.model));

  /**
   * 流里每跑完一步就报一次用量 —— 圆环实时跟着涨（一轮多步时也能看出来
   * 上下文在长）。累计值在这里往上加；下一次 `/api/session` 回来整份换成
   * 服务端的（两边数的是同一批步骤，对得上）。
   */
  $effect(() => {
    onUsage((u) => {
      const t = usage?.total ?? { input: 0, output: 0, cached: 0, calls: 0 };
      usage = {
        last: u,
        total: {
          input: t.input + u.prompt_tokens,
          output: t.output + u.completion_tokens,
          cached: t.cached + (cachedTokens(u) ?? 0),
          calls: t.calls + 1,
        },
      };
    });
    return () => onUsage(null);
  });

  /**
   * 本地正连着的那条流。**只属于当前看着的会话**。
   *
   * 可以同时在几个会话里各跑一个回合（服务端按会话记）。切走的时候只是把
   * 本地这条流断开（`detach`），服务端那个回合照跑；切回来靠轮询看进度。
   */
  type Run = { controller: AbortController };
  let live: Run | null = null;

  /** 不看这个会话了：断开本地的流，**不**停服务端的回合。 */
  function detach() {
    const r = live;
    if (!r) return;
    live = null;
    busy = false;
    r.controller.abort();
  }

  /**
   * 点了「新对话」、但第一句还没发出去。
   *
   * 这时服务端**还没有**这个会话 —— 不这样就会变成：连点五次「新对话」，
   * 抽屉里多出五条「新对话」空记录。所以它只是一个本地意图，
   * 第一句话发出去之前才在服务端建（见 `ensureSession`）。
   */
  let fresh = $state(false);

  let drawer = $state(false);
  /**
   * 宽屏下侧边栏要不要展开（窄屏不看它 —— 那里侧边栏是抽屉，看 `drawer`）。
   *
   * 默认**收起**：刷新之后永远是同一个样子（不会有“从关闭弹到打开”那一下），
   * 要用再点左上的菜单。不持久化 —— 这个 App 什么都不往 localStorage 里存。
   */
  let sidebar = $state(false);
  let sheet = $state(false);
  /** 「选择机器」面板（侧栏底栏那个机器标签点开的，见 MachineSheet） */
  let machineSheet = $state(false);
  /** 壳在不在（能在手机核心 / 电脑核心之间切）。桌面浏览器、手机浏览器里没有壳 */
  const canSwitch = canSwitchCore();
  /** 设置页。可以从抽屉的齿轮进，也可以从模型面板「模型提供商」页的「管理模型提供商」进 ——
   *  后者是叠在模型面板上面的，关掉之后回到模型面板。每层各自在
   *  lib/overlay.ts 里登记，返回键先关最上面那层。 */
  let settings = $state(false);
  /** 设置页每关一次加一：Key 可能改了，模型面板据此重拉清单 */
  let settingsRev = $state(0);
  const closeSettings = () => {
    settings = false;
    settingsRev++;
  };

  /**
   * 开机就把模型清单拉回来。
   *
   * 「选择模型」面板是「第一帧就得有数据」才不跳（见 `lib/models.svelte.ts`）——
   * 等用户点开才拉的话，第一次打开还是会先渲染「载入中…」再长高。这里提前
   * 拉一次，之后 10 分钟内打开面板都是瞬间的，一个请求也不发。
   *
   * 没配服务商就不拉（`/api/models` 也没东西可回）。
   */
  $effect(() => {
    const on = !!store.llm;
    const r = settingsRev;
    if (!on) return;
    // `untrack`：同 ModelSheet 里的理由，别把 `load()` 内部读的 `$state` 也变成依赖
    void untrack(() => models.load(r));
  });

  /**
   * agent 有没有开着浏览器 —— 有才显示头部那个地球按钮。
   *
   * 状态在**壳**那边（`AgentApp.browser.hasPage()`，见 BrowserController）：
   * agent 调 `browser` 工具会懒建一只 WebView，把标签关光又回到「没有」。
   * 所以两条腿：挂载时同步拉一次（`AgentHost.hasBrowser`），之后壳推
   * `window.__browserAvailable`（见 MainActivity 的 `pushBrowserAvailable`）。
   */
  let browserAvailable = $state(false);
  $effect(() => {
    // 先挂上推送口，再拉一次初始值：同一个 tick 里不会漏掉中间那一下
    window.__browserAvailable = (on) => {
      browserAvailable = !!on;
    };
    browserAvailable = agentHasBrowser();
    return () => {
      delete window.__browserAvailable;
    };
  });

  /** 输入区实际高度。消息列表的底部留白和底部渐隐都跟着它走。 */
  let composerH = $state(0);

  // —— 会话 ——
  //
  // 消息本来只活在 JS 内存里，Activity 重建 / 刷新 / 从后台回来就空了。
  // 服务端一直留着，所以进来先拉一次。
  async function loadMessages() {
    // 本地流正在跑：本地才是最新的，服务端那份是旧的。拉回来只会白传
    // 一整段历史再丢掉 —— 干脆不发这个请求。
    if (busy) return;
    // 刚点了「新对话」还没发第一句：服务端那边还是旧会话（新的根本还没建），
    // 拉回来会把刚清空的界面又填满旧内容。
    if (fresh) return;
    try {
      const r = await fetch(`/api/session?rev=${encodeURIComponent(rev)}`);
      if (!r.ok) return;
      const d = (await r.json()) as {
        messages?: Message[];
        busy: boolean;
        unchanged?: boolean;
        rev?: string;
        /** 增量：只给了「当前回合」那一段 */
        append?: boolean;
        /** 增量时从哪里开始替换 */
        replaceFrom?: number;
        /** 服务端队列里等着交给模型的那几条（`Message` 的形状，`done: false`） */
        queued?: Message[];
        /**
         * 增量且是同一回合的延续：`messages[0]` 接在本地第 `replaceFrom` 条
         * （assistant）的前 `keepParts` 个 part 后面 —— 服务端只重发最后一步，
         * 不再把整个回合重发一遍（见 core 的 `session_patch`）。
         */
        keepParts?: number | null;
        /** token 用量（输入框那个圆环）。服务端那份是权威 */
        usage?: (Partial<UsageTotals> & { last?: Usage | null }) | null;
      };
      serverBusy = !!d.busy;
      rev = d.rev ?? '';
      // 排队那几条按服务端**整份重来**（`unchanged` 时也要）：它们的变化不进
      // `rev`（那是消息的版本号），光看 rev 看不出「排队里少了一条」。
      // `pending` 的那几条是刚发出去、还没听到回话的，留着 —— 服务端下一份就会
      // 报上它们（或者它们已经开跑了）。
      // 会话 id 从 rev 里解（`会话id:消息数:更新时间`）。**不能读 `sessionId`**：
      // 它要到下面几行才赋值，而排队那几条得按会话过滤 —— 刚刷新进来的那一份
      // 会话 id 在这里还是空的（那几条就全被滤掉了，实测踩过）。
      const sid = rev.split(':')[0] || sessionId;
      if (d.queued) {
        const inflight = queue.filter((q) => q.pending && q.sid === sessionId);
        queue = [
          ...d.queued.map((m): Queued => ({ id: m.id, sid, parts: m.parts })),
          ...inflight,
        ];
      }
      // 服务端说没变：连 JSON 里都不带 messages，什么都不用做
      if (d.unchanged) return;
      // 用量整份换成服务端的：它数的是这个会话的**全部**调用，而我们手上
      // 那份只从页面加载后开始算。`last` 没有 = 这个会话还没跑过
      usage = d.usage?.last
        ? {
            last: d.usage.last,
            total: {
              input: d.usage.input ?? 0,
              output: d.usage.output ?? 0,
              cached: d.usage.cached ?? 0,
              calls: d.usage.calls ?? 0,
            },
          }
        : null;
      // 会话 id 变了（切会话 / 重置）就不能按 id 复用旧消息 ——
      // `u1` 这种 id 每条会话都会有。`sid` 在上面算过了。
      const same = sid === sessionId;
      if (d.append && same) {
        // 服务端只补了差的那段：前面的原样留着，从 replaceFrom 换掉尾部
        const from = Math.min(d.replaceFrom ?? 0, messages.length);
        const fresh = d.messages ?? [];
        const keep = d.keepParts;
        if (keep != null) {
          const old = messages[from];
          if (!old || old.role !== 'assistant' || old.parts.length < keep || !fresh[0]) {
            // 本地那条和服务端以为的对不上：老老实实要一份全量
            rev = '';
            return loadMessages();
          }
          fresh[0] = { ...fresh[0], parts: [...old.parts.slice(0, keep), ...fresh[0].parts] };
        }
        messages = [...messages.slice(0, from), ...fresh];
      } else {
        messages = same ? reconcile(messages, d.messages ?? []) : (d.messages ?? []);
      }
      sessionId = sid;
    } catch {
      /* 拉不到就算了 */
    }
  }

  /**
   * 上次拿到的会话版本号（服务端算的）。一致就不再重传整段历史。
   *
   * **切会话 / 新建 / 删除后必须清掉** —— 否则会拿旧会话的 rev 去比对新会话，
   * 服务端一旦碰巧返回 unchanged，界面就卡在上一个会话的内容上。
   */
  let rev = '';

  /**
   * 当前会话 id（从 rev 里解出来）。切会话时清掉，避免跨会话复用消息对象。
   *
   * `$state`：**队列要跟着它走**（输入框上面那几条「排队中」只看当前会话的，
   * 切回去才轮到它发）。原来是普通的 let —— 那时没人从模板里读它，现在有了。
   */
  let sessionId = $state('');

  /**
   * 能复用的旧消息对象原样留着，Svelte 就不会重新渲染它们的 DOM。
   *
   * 服务端的会话是**只追加**的：一条消息 append 之后内容不再变。唯一的例外
   * 是最后一条 assistant —— 一个回合里它会把多步的 parts 合进来、越变越长。
   * 所以除了最后一条，能按 id 对上的消息直接复用；最后一条永远用新的。
   *
   * 以前这里比的是 `JSON.stringify(o) === JSON.stringify(m)`：轮询每 1.5 秒
   * 就要把**整段历史**（包括每个 50KB 的 bash 输出）序列化两遍 —— 长任务
   * 轮询时这是最大的一笔 CPU 和 GC。id 是位置标识，只追加的情况下对 id
   * 就够了。
   */
  function reconcile(prev: Message[], next: Message[]): Message[] {
    // 切会话 / 清空 / 换了一条会话：不能复用
    if (next.length < prev.length) return next;
    if (prev.length > 0 && next.length > 0 && prev[0].id !== next[0].id) return next;

    // 只复用「上次就已经不是最后一条」的消息：上次的最后一条当时可能还在长
    // （一个回合里 assistant 消息会把多步 parts 合进来），它保存的可能是半成品。
    const out = next.slice();
    const prevLast = prev.length - 1;
    for (let i = 0; i < Math.min(prev.length, next.length); i++) {
      if (i < prevLast && prev[i].id === next[i].id) out[i] = prev[i];
    }
    return out;
  }

  async function loadSessions() {
    try {
      const r = await fetch('/api/sessions');
      if (r.ok) sessions = ((await r.json()) as { sessions: Session[] }).sessions ?? [];
    } catch {
      /* 同上 */
    }
  }

  function newSession() {
    detach();
    // **不请求服务端**：新会话到第一句话才建（`ensureSession`），
    // 这里只把本地视图清空 —— 点了不发就不会攒出一堆空会话。
    fresh = true;
    rev = '';
    sessionId = '';
    messages = [];
    // 新会话还没跑过，用量也从零开始
    usage = null;
    // 新会话在服务端**还不存在**，所以它一定没在跑 —— 不清的话会留着上一条
    // 会话的「服务端还在跑」，输入框要等一个轮询周期才解冻。
    serverBusy = false;
    drawer = false;
  }

  async function switchSession(id: string) {
    detach();
    await fetch('/api/sessions/activate', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ id }),
    });
    fresh = false;
    rev = '';
    sessionId = '';
    await Promise.all([loadSessions(), loadMessages()]);
    drawer = false;
  }

  async function renameSession(id: string, title: string) {
    await fetch('/api/sessions/rename', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ id, title }),
    });
    await loadSessions();
  }

  async function deleteSession(id: string) {
    if (id === sessionId) detach();
    // 这个会话没了，它排着的队也没地方发了
    queue = queue.filter((q) => q.sid !== id);
    await fetch('/api/sessions/delete', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ id }),
    });
    fresh = false;
    rev = '';
    sessionId = '';
    await Promise.all([loadSessions(), loadMessages()]);
  }

  $effect(() => {
    void loadMessages();
    void loadSessions();
  });

  // 切回前台 / 重新聚焦 → 同步一次。
  //
  // 为什么需要：App 切到后台时 WebView 会挂起 JS，SSE 流可能就断了。
  // 但**服务端的 agent 不会停**（前台服务保着进程，Rust 线程照跑，
  // SSE 客户端断开只是让 tx.send 失败然后被忽略）。
  // 所以切回来时从服务端拉最新状态，而不是相信本地的旧视图。
  $effect(() => {
    const sync = () => {
      if (document.visibilityState !== 'visible') return;
      void loadMessages();
      void loadSessions();
    };
    document.addEventListener('visibilitychange', sync);
    window.addEventListener('focus', sync);
    window.addEventListener('pageshow', sync);
    return () => {
      document.removeEventListener('visibilitychange', sync);
      window.removeEventListener('focus', sync);
      window.removeEventListener('pageshow', sync);
    };
  });

  // 服务端还在跑 → 轮询到结束。
  //
  // 比"重连 SSE"简单得多，而且跨 WebView 挂起也可靠。
  // 服务端每个步骤完成都会把 assistant 消息写进会话，所以轮询能
  // 看到逐步骤推进。
  /** 轮询间隔（ms）。服务端的粒度是「一步」，1.5 秒完全够用；再密就是白转发。 */
  const POLL_MS = 1500;
  /** 按下停止之后过多久拉一次（给服务端一点时间把「已停止」的工具结果补上） */
  const AFTER_STOP_MS = 300;
  $effect(() => {
    if (!serverBusy) return;
    const t = setInterval(() => void loadMessages(), POLL_MS);
    return () => clearInterval(t);
  });

  /**
   * 服务端把排队的消息交给模型了 → 把那几条从「排队中」挪进正文。
   *
   * 挪进去就成了本地历史的一部分（`done: true`）：服务端那边它们也确实是历史了
   * —— 紧接着流里会来一个 `start`，模型的回答进**新的一条**助手消息（`consume`
   * 的 `onRestart`），和服务端历史的形状一致（`session_view` 里 User 会打断
   * 助手消息的合并）。
   */
  $effect(() => {
    onQueuedDelivered((ids) => {
      if (!ids.length) return;
      const moved = queue.filter((q) => ids.includes(q.id));
      if (moved.length) queue = queue.filter((q) => !ids.includes(q.id));
      for (const q of moved) {
        messages.push({ id: q.id, role: 'user', parts: q.parts, steps: 0, done: true });
      }
    });
    return () => onQueuedDelivered(null);
  });

  // 当前会话里排队的那几条（其它会话的排着队，切回去才看得见）。
  let queued = $derived(queue.filter((q) => q.sid === sessionId));

  // 它们按「**还没发出去**的用户消息」渲染（`done: false`）—— 消息列表里就是
  // 普通用户气泡的样子，只多一圈虚线和左边一个 ×（详见上面 Queued 的注释）。
  let queuedMsgs = $derived(
    queued.map(
      (q): Message => ({ id: q.id, role: 'user', parts: q.parts, steps: 0, done: false }),
    ),
  );

  // 左上角那个菜单按钮：窄屏是开抽屉；宽屏是**收 / 展**侧边栏
  // （宽屏侧边栏一直在，只是可以收起来把地方让给对话）。
  //
  // 展开时**显式**刷新列表。不能只靠 `$effect(() => { if (drawer) ... })`：
  // 开着的时候状态没变化，effect 不会重跑 —— 实测踩过（在别处新建了会话，
  // 抽屉里还是“还没有对话”）。
  function toggleSidebar() {
    if (wide.on) {
      sidebar = !sidebar;
      if (!sidebar) return;
    } else {
      drawer = true;
    }
    void loadSessions();
    void loadMessages();
  }

  // 返回键关覆盖层：每个覆盖层自己登记（lib/overlay.ts），这里不管。
  const close = () => {
    drawer = false;
    sheet = false;
  };

  // —— 对话 ——

  /**
   * 收一条流：把 chunk 一个个喂进 `msg`，直到流结束。
   *
   * `me` 是这条流的身份（`live` 和停止键认它）；`msg` 是这条流往里写的消息 ——
   * 服务端把排队的消息交给模型时会再来一个 `start`，那时换**新的一条**写
   * （见下面 `onRestart`），和服务端历史的形状一致。
   */
  async function run(me: Run, msg: Message, stream: AsyncGenerator<UiChunk, void, undefined>) {
    live = me;
    busy = true;
    let cur = msg;
    try {
      await consume(msg, stream, undefined, (prev) => {
        prev.done = true;
        cur = store.beginStreaming();
        messages.push(cur);
        return cur;
      });
    } catch (e) {
      if (!me.controller.signal.aborted) {
        cur.parts.push({ type: 'error', text: String(e) });
      }
    } finally {
      cur.done = true;
      // 已经被 detach（切走了）、或者后面又开了一条：别把别人的状态清掉
      if (live === me) {
        live = null;
        busy = false;
      }
      void loadSessions();
    }
  }

  /** 新会话到第一句话才在服务端建（点了「新对话」不发就不留痕）。 */
  async function ensureSession(): Promise<string | null> {
    if (!fresh && sessionId) return sessionId;
    try {
      const r = await fetch('/api/sessions', { method: 'POST' });
      if (!r.ok) return null;
      const { id } = (await r.json()) as { id: string };
      fresh = false;
      sessionId = id;
      rev = '';
      return id;
    } catch {
      return null;
    }
  }

  /**
   * 发一条消息。
   *
   * **谁跑谁排队由服务端说了算**，前端不猜：先按「排队中」画出来，然后看
   * `POST /api/chat` 的**首帧** ——
   *
   *   - 正常流（`start` 开头）= 这个回合开跑了 → 那条气泡翻进正文，接着收流；
   *   - `data-queued` = 这个会话正跑着，消息进了**服务端的**队列 → 虚线气泡留着，
   *     等回合跑到步边界时服务端会把它交给模型（`data-queued-delivered`）。
   *
   * 为什么不再由前端判断「正跑着 → 排队」：那样队列就活在 WebView 里，App 切到
   * 后台、WebView 被冻住或回收，这条消息永远发不出去；而且只有等整个回合结束
   * 才发得出去，模型中途空下来也拿不到它。
   */
  async function send(attachments: Upload[]) {
    const text = input.trim();
    if (!text && attachments.length === 0) return;
    input = '';

    const id = uid();
    const parts = userParts(text, attachments);
    const sid = await ensureSession();
    if (!sid) {
      messages.push({ id, role: 'user', parts, steps: 0, done: true });
      messages.push({ id: uid(), role: 'assistant', steps: 0, done: true, parts: [{ type: 'error', text: '建会话失败，再试一次' }] });
      return;
    }
    queue = [...queue, { id, sid, parts, pending: true }];
    await deliver(id, sid, text, attachments, parts);
  }

  /**
   * 一条用户消息的 parts：正文 + 附件。
   *
   * 发出去的那条（`deliver`）和排队的那条（`queuedMsgs`）用的都是它 ——
   * 所以排队中那条能直接当用户消息渲染，长得和发出去的一模一样。
   */
  function userParts(text: string, attachments: Upload[]): Part[] {
    return [
      { type: 'text', id: 'u', text },
      ...attachments.map(
        (a, i): Part =>
          a.kind === 'image'
            ? { type: 'image', id: `a${i}`, url: a.url, name: a.name, mime: a.mime }
            : { type: 'file', id: `a${i}`, url: a.url, name: a.name, mime: a.mime },
      ),
    ];
  }

  /**
   * 真的把一条消息交给服务端（首帧决定它是「开跑」还是「排队」）。
   */
  async function deliver(id: string, sid: string, text: string, attachments: Upload[], parts: Part[]) {
    const me: Run = { controller: new AbortController() };
    const stream = streamFrom(
      '/api/chat',
      { text, id, session: sid, attachments: attachments.map((a) => a.id) },
      me.controller.signal,
    );
    let first: UiChunk | null = null;
    try {
      first = (await stream.next()).value ?? null;
    } catch (e) {
      // 连请求都没发出去：把那条虚线气泡撤了，用一条错误消息说清楚（字不能丢）
      queue = queue.filter((q) => q.id !== id);
      messages.push({ id, role: 'user', parts, steps: 0, done: true });
      messages.push({
        id: uid(),
        role: 'assistant',
        steps: 0,
        done: true,
        parts: [{ type: 'error', text: String(e) }],
      });
      return;
    }

    if (isQueued(first)) {
      // 排上了：服务端那边已经有它了，这条流到此为止（读完就扔，别挂着连接）
      queue = queue.map((q) => (q.id === id ? { ...q, pending: false } : q));
      void (async () => {
        try {
          for await (const _ of stream) void _;
        } catch {
          /* 一帧就结束的流，读不完也无所谓 */
        }
      })();
      // 本地这条流结束了 ≠ 服务端那个回合结束了：此刻还在跑的是**另一条**流
      // （或者根本没连着）。所以这里 busy 一点都不动。
      return;
    }

    // 开跑了：那条虚线气泡翻进正文（换了个 id 的影子都没有 —— 就是同一个 id）
    queue = queue.filter((q) => q.id !== id);
    messages.push({ id, role: 'user', parts, steps: 0, done: true });
    const msg = store.beginStreaming();
    messages.push(msg);
    await run(me, msg, prepend(first, stream));
  }

  /**
   * 撤掉一条**还在排队**的消息（虚线气泡左边那个 ×）。
   *
   * 队列在服务端，所以得告诉它 —— 不然那条照样会在下一个步边界交给模型。界面上
   * 立刻撤掉（不等回话）：真撤不掉（刚好在这一刻被交出去了）也没关系，下一次
   * `loadMessages` 就看到它已经落进正文了。
   */
  function dropQueued(id: string) {
    const item = queue.find((q) => q.id === id);
    queue = queue.filter((q) => q.id !== id);
    if (!item) return;
    void fetch('/api/sessions/unqueue', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ session: item.sid, id }),
    }).catch(() => {
      /* 撤不掉就算了：它已经落进正文，或者下次轮询会纠正 */
    });
  }

  const stop = () => {
    // 服务端循环看不到 SSE 断连（tx.send 失败会被忽略），得显式告诉它停。
    // 否则前端显示停了，agent 还在后台一轮轮跑工具、烧 token。
    // 只停**这个**会话的回合：别的会话在跑的不受影响。
    void fetch('/api/stop', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(sessionId ? { session: sessionId } : {}),
    });
    const r = live;
    live = null;
    busy = false;
    r?.controller.abort();
    // 本地流断了，服务端收尾（补上「已停止」的工具结果）的那几条要拉回来；
    // 没连着流时按的停止也一样：马上拉一次，「运行中」尽快消失
    setTimeout(() => void loadMessages(), AFTER_STOP_MS);
  };
</script>

<div class="flex h-full overflow-hidden bg-ink">
  <!-- 宽屏：侧边栏**并排**在左边（不是抽屉），默认收着，点左上角的菜单展开。
       窄屏：它还是盖在对话上的抽屉，同样是那个菜单按钮控制（见下面）。 -->
  {#if wide.on && sidebar}
    <Drawer
      {sessions}
      onclose={close}
      onswitch={switchSession}
      ondelete={deleteSession}
      onrename={renameSession}
      onsettings={() => (settings = true)}
      machine={store.machine}
      onmachine={canSwitch ? () => (machineSheet = true) : null}
      pinned
    />
  {/if}

  <!-- 对话区：把侧边栏剩下的宽度**全部**拿走（不限宽）。 -->
  <div
    class="shell relative h-full w-full min-w-0 flex-1 overflow-hidden"
    style:--composer-h="{composerH}px"
  >
    <!-- 消息列表铺满整个高度：内容从头部和输入卡片下面穿过去。
         顶部/底部的渐隐是 `.shell` 的伪元素（只为了装饰，不值得占两个 div）。 -->
    <!-- 不要给 main 加 z-index：`z-0` 会形成层叠上下文，把里面
         弹出面板的 z-50 困住，压不过同级 z-20 的输入框（踩过）。
         z-auto 的定位元素不建层叠上下文，浮层的 z-10 依旧在它上面。 -->
    <main class="absolute inset-0">
      {#if messages.length === 0}
        <h1 class="grid h-full place-items-center text-2xl font-normal">来思考些什么?</h1>
      {:else}
        <MessageList {messages} pending={queuedMsgs} ondrop={dropQueued} />
      {/if}
    </main>

    <!--
      渐隐用浮层而不是滚动容器上的 mask。
      mask 会让滚动容器每帧重新合成 —— 实测手机上划动明显卡顿。
    -->

    <!-- 头部浮在内容上 -->
    <header
      class="pointer-events-none absolute inset-x-0 top-0 z-20 flex items-center justify-between
             px-4 pb-1 pt-[calc(0.25rem+env(safe-area-inset-top))]"
    >
      <!-- 窄屏：开抽屉；宽屏：收 / 展侧边栏（两个宽度下都在） -->
      <button
        onclick={toggleSidebar}
        aria-label="菜单"
        title={wide.on ? '收起 / 展开侧边栏' : '菜单'}
        class="pointer-events-auto p-1 text-fg"
      >
        <Icon name="menu" size={20} />
      </button>
      <!-- 右上角 = 新对话（对齐 Claude 移动端：左边菜单，右边开新会话）。 -->
      <div class="pointer-events-auto ml-auto flex items-center gap-1">
        <!-- agent 的浏览器：它自己在后台开了网页时，用户可以看看它在干什么、
             也能自己接着操作（壳里那块是**同一只** WebView）。
             **只在它真开着页面时才出现** —— 没开过页面时点它只会弹一句
             「还没打开过网页」，那就不该显示（也少一个常驻图标）。 -->
        {#if browserAvailable}
          <button
            onclick={openAgentBrowser}
            aria-label="看 agent 的浏览器"
            title="看 agent 的浏览器"
            class="p-1 text-fg"
          >
            <Icon name="globe" size={20} />
          </button>
        {/if}
        <button onclick={newSession} aria-label="新对话" title="新对话" class="p-1 text-fg">
          <Icon name="newchat" size={20} />
        </button>
      </div>
    </header>

    <!-- 输入卡片也浮在内容上 -->
    <div class="absolute inset-x-0 bottom-0 z-20" bind:offsetHeight={composerH}>
      <!-- 排队中那几条不在这里（输入框上方）—— 它们画在**消息列表末尾**，
           和已发送的用户气泡同一个样子（虚线 + 左边一个 ×），见 MessageList。 -->

      <Composer
        bind:value={input}
        busy={busy || serverBusy}
        {usage}
        {ctxWindow}
        onsend={send}
        onstop={stop}
        onmodel={() => (sheet = true)}
      />
    </div>
  </div>

  <!-- 窄屏：抽屉盖在对话上（宽屏它已经并排在左边了） -->
  {#if !wide.on && drawer}
    <Drawer
      {sessions}
      onclose={close}
      onswitch={switchSession}
      ondelete={deleteSession}
      onrename={renameSession}
      onsettings={() => (settings = true)}
      machine={store.machine}
      onmachine={canSwitch ? () => (machineSheet = true) : null}
    />
  {/if}
</div>

{#if sheet}<ModelSheet onclose={close} onconfig={() => (settings = true)} rev={settingsRev} />{/if}

{#if machineSheet}<MachineSheet onclose={() => (machineSheet = false)} />{/if}

{#if settings}<Settings onclose={closeSettings} />{/if}

{#if viewer.url}<Viewer />{/if}
