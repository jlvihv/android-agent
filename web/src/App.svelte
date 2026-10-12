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
  import { consume, streamFrom, type Message, type Part, type Upload } from './lib/sse';
  import { uid } from './lib/uid';
  import Icon from './components/Icon.svelte';
  import Composer from './components/Composer.svelte';
  import MessageList from './components/MessageList.svelte';
  import Drawer, { type Session } from './components/Drawer.svelte';
  import ModelSheet from './components/ModelSheet.svelte';
  import Settings from './components/Settings.svelte';
  import Viewer from './components/Viewer.svelte';
  import { viewer } from './lib/viewer.svelte.ts';
  import { models } from './lib/models.svelte.ts';
  import { agentHasBrowser, openAgentBrowser } from './lib/overlay';

  let input = $state('');
  /** 当前看着的这个会话，本地正连着它的流。 */
  let busy = $state(false);
  /** 服务端说当前会话还在跑。App 切到后台、或者从别的会话切回来时，
   *  本地没连着流，靠它判断"还在进行中"（然后轮询）。 */
  let serverBusy = $state(false);
  let messages = $state<Message[]>([]);
  let sessions = $state<Session[]>([]);

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
  let sheet = $state(false);
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
      };
      serverBusy = !!d.busy;
      // 服务端说没变：连 JSON 里都不带 messages，什么都不用做
      if (d.unchanged) return;
      rev = d.rev ?? '';
      // rev 的格式是 `会话id:消息数:更新时间`。会话 id 变了（切会话 / 重置）
      // 就不能按 id 复用旧消息 —— `u1` 这种 id 每条会话都会有。
      const sid = rev.split(':')[0];
      const same = sid === sessionId;
      if (d.append && same) {
        // 服务端只重发了「当前回合」那段：前面的原样留着，从 replaceFrom 换掉尾部
        const from = Math.min(d.replaceFrom ?? 0, messages.length);
        messages = [...messages.slice(0, from), ...(d.messages ?? [])];
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

  /** 当前会话 id（从 rev 里解出来）。切会话时清掉，避免跨会话复用消息对象。 */
  let sessionId = '';

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
  $effect(() => {
    if (!serverBusy) return;
    const t = setInterval(() => void loadMessages(), 1500);
    return () => clearInterval(t);
  });

  // 打开抽屉时**显式**刷新列表。
  //
  // 不能只靠 `$effect(() => { if (drawer) ... })`：抽屉一直开着的时候
  // drawer 状态没变化，effect 不会重跑 —— 实测踩过（在别处新建了会话，
  // 抽屉里还是"还没有对话"）。
  /** 横幅里只显示主机名：URL 可能很长，整条塞不下 */
  const hostOf = (url: string): string => {
    try {
      return new URL(url).host;
    } catch {
      return url;
    }
  };

  function openDrawer() {
    drawer = true;
    void loadSessions();
    void loadMessages();
  }

  // 返回键关覆盖层：每个覆盖层自己登记（lib/overlay.ts），这里不管。
  const close = () => {
    drawer = false;
    sheet = false;
  };

  // —— 对话 ——

  async function run(msg: Message, path: string, body: unknown) {
    const me: Run = { controller: new AbortController() };
    live = me;
    busy = true;
    try {
      await consume(msg, streamFrom(path, body, me.controller.signal));
    } catch (e) {
      if (!me.controller.signal.aborted) msg.parts.push({ type: 'error', text: String(e) });
    } finally {
      msg.done = true;
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

  async function send(attachments: Upload[]) {
    const text = input.trim();
    // serverBusy：本地没连着流（从后台 / 别的会话切回来），但这个会话的回合
    // 还在跑。这时再发一条，服务端会回 409（一个会话同时只跑一个回合）。
    if ((!text && attachments.length === 0) || busy || serverBusy) return;
    input = '';

    // 附件先作为 part 拼进本地气泡（和 session_view 恢复出来的形状一致）——
    // 否则发出去的下一秒，自己的图片就看不见了。
    const parts: Part[] = [
      { type: 'text', id: 'u', text },
      ...attachments.map(
        (a, i): Part =>
          a.kind === 'image'
            ? { type: 'image', id: `a${i}`, url: a.url, name: a.name, mime: a.mime }
            : { type: 'file', id: `a${i}`, url: a.url, name: a.name, mime: a.mime },
      ),
    ];

    messages.push({
      id: uid(),
      role: 'user',
      parts,
      steps: 0,
      done: true,
    });
    const msg = store.beginStreaming();
    messages.push(msg);
    busy = true; // 建会话那一下也算在跑：别让人连点两次
    const sid = await ensureSession();
    if (!sid) {
      busy = false;
      msg.parts.push({ type: 'error', text: '建会话失败，再试一次' });
      msg.done = true;
      return;
    }
    void run(msg, '/api/chat', {
      text,
      // 明确写进哪个会话：回合在后台跑着时用户可能已经切走了
      session: sid,
      attachments: attachments.map((a) => a.id),
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
    setTimeout(() => void loadMessages(), 300);
  };
</script>

<div class="shell relative h-full overflow-hidden bg-ink" style:--composer-h="{composerH}px">
  <!-- 消息列表铺满整个高度：内容从头部和输入卡片下面穿过去。
       顶部/底部的渐隐是 `.shell` 的伪元素（只为了装饰，不值得占两个 div）。 -->
  <!-- 不要给 main 加 z-index：`z-0` 会形成层叠上下文，把里面
       弹出面板的 z-50 困住，压不过同级 z-20 的输入框（踩过）。
       z-auto 的定位元素不建层叠上下文，浮层的 z-10 依旧在它上面。 -->
  <main class="absolute inset-0">
    {#if messages.length === 0}
      <h1 class="grid h-full place-items-center text-2xl font-normal">来思考些什么?</h1>
    {:else}
      <MessageList {messages} />
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
    <button onclick={openDrawer} aria-label="菜单" class="pointer-events-auto p-1 text-fg">
      <Icon name="menu" size={20} />
    </button>
    <!-- 右上角 = 新对话（对齐 Claude 移动端：左边菜单，右边开新会话）。
         设备状态点挪进抽屉左下角的设置按钮（作为角标）。 -->
    <div class="pointer-events-auto flex items-center gap-1">
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
      <button
        onclick={newSession}
        aria-label="新对话"
        title="新对话"
        class="p-1 text-fg"
      >
        <Icon name="newchat" size={20} />
      </button>
    </div>
  </header>

  {#if store.openUrl}
    {@const pending = store.openUrl}
    <!-- 沙箱里有人要开浏览器（agent-open → /api/open）。自动弹窗可能被 WebView
         的弹窗策略拦掉，所以留一条可点的横幅：点一下一定走系统浏览器。 -->
    <div
      class="absolute inset-x-0 top-11 z-30 mx-auto flex max-w-md items-center gap-3 rounded-xl
             border border-hairline bg-control px-3 py-2 text-meta shadow-lg"
    >
      <span class="min-w-0 flex-1 truncate text-muted" title={pending.url}>
        {pending.opened ? '已在浏览器打开' : '要在浏览器里打开'}
        {hostOf(pending.url)}
      </span>
      <button class="shrink-0 text-ok" onclick={() => store.openExternal(pending.url)}>打开</button>
      <button
        class="shrink-0 text-faint"
        onclick={() => store.dismissOpenUrl()}
        aria-label="关闭">✕</button
      >
    </div>
  {/if}

  <!-- 输入卡片也浮在内容上 -->
  <div class="absolute inset-x-0 bottom-0 z-20" bind:offsetHeight={composerH}>
    {#if serverBusy && !busy}
      <!-- 服务端在跑但本地没连着流（刚从后台回来）-->
      <div class="flex items-center gap-2 px-5 pb-1 text-xs text-muted">
        <span class="size-1.5 animate-pulse rounded-full bg-warn"></span>
        agent 正在运行…
      </div>
    {/if}

    <Composer
      bind:value={input}
      busy={busy || serverBusy}
      onsend={send}
      onstop={stop}
      onmodel={() => (sheet = true)}
    />
  </div>
</div>

{#if drawer}
  <Drawer
    {sessions}
    onclose={close}
    onnew={newSession}
    onswitch={switchSession}
    ondelete={deleteSession}
    onrename={renameSession}
    onsettings={() => (settings = true)}
  />
{/if}

{#if sheet}<ModelSheet onclose={close} onconfig={() => (settings = true)} rev={settingsRev} />{/if}

{#if settings}<Settings onclose={closeSettings} />{/if}

{#if viewer.url}<Viewer />{/if}
