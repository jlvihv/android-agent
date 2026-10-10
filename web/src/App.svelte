<script lang="ts">
  /**
   * 外壳。结构对齐 Claude 移动端：
   *   header（菜单 + 状态）/ main（空态标题 或 消息列表）/ 输入卡片
   *   抽屉 = Claude 侧边栏（导航项 + 最近对话 + 新建会话）
   *
   * 没有魔法数字：尺寸走 Tailwind 刻度，顶部/底部由安全区给。
   */
  import { store } from './lib/store.svelte.ts';
  import { consume, streamFrom, type Message, type ToolPart } from './lib/sse';
  import { uid } from './lib/uid';
  import Icon from './components/Icon.svelte';
  import Composer from './components/Composer.svelte';
  import MessageList from './components/MessageList.svelte';
  import Drawer, { type Session } from './components/Drawer.svelte';
  import LlmSheet from './components/LlmSheet.svelte';

  let input = $state('');
  let busy = $state(false);
  /** 服务端还在跑。App 切到后台时 agent 不停，切回来靠它判断"还在进行中"。 */
  let serverBusy = $state(false);
  let messages = $state<Message[]>([]);
  let sessions = $state<Session[]>([]);
  let controller: AbortController | null = null;

  let drawer = $state(false);
  let sheet = $state(false);

  /** 输入区实际高度。消息列表的底部留白和底部渐隐都跟着它走。 */
  let composerH = $state(0);

  const dot = $derived(
    !store.connected ? 'bg-control' : store.device?.connected ? 'bg-accent' : 'bg-warn',
  );

  // —— 会话 ——
  //
  // 消息本来只活在 JS 内存里，Activity 重建 / 刷新 / 从后台回来就空了。
  // 服务端一直留着，所以进来先拉一次。
  async function loadMessages() {
    try {
      const r = await fetch('/api/session');
      if (!r.ok) return;
      const raw = await r.text();
      const d = JSON.parse(raw) as { messages: Message[]; busy: boolean };
      serverBusy = !!d.busy;
      // 本地流还在跑就别覆盖，否则会把正在流式的消息对象换掉
      if (busy || raw === lastRaw) return;
      lastRaw = raw;
      messages = reconcile(messages, d.messages ?? []);
    } catch {
      /* 拉不到就算了 */
    }
  }

  /** 上次拉到的原样响应。轮询时没变化就什么都不做。 */
  let lastRaw = '';

  /**
   * 能复用的旧消息对象原样留着。
   *
   * 轮询每 1.5 秒拉一次整份会话，直接整体替换的话每条消息都是新对象，
   * 所有 markdown 全部重新解析、重新渲染 —— 跑长任务时划动明显掉帧。
   * 内容没变的消息留旧对象，Svelte 就不会碰它们的 DOM。
   */
  function reconcile(prev: Message[], next: Message[]): Message[] {
    const old = new Map(prev.map((m) => [m.id, m]));
    return next.map((m) => {
      const o = old.get(m.id);
      return o && JSON.stringify(o) === JSON.stringify(m) ? o : m;
    });
  }

  async function loadSessions() {
    try {
      const r = await fetch('/api/sessions');
      if (r.ok) sessions = ((await r.json()) as { sessions: Session[] }).sessions ?? [];
    } catch {
      /* 同上 */
    }
  }

  async function newSession() {
    await fetch('/api/sessions', { method: 'POST' });
    await Promise.all([loadSessions(), loadMessages()]);
    drawer = false;
  }

  async function switchSession(id: string) {
    await fetch('/api/sessions/activate', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ id }),
    });
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
    await fetch('/api/sessions/delete', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ id }),
    });
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
    busy = true;
    controller = new AbortController();
    try {
      await consume(msg, streamFrom(path, body, controller.signal));
    } catch (e) {
      if (!controller?.signal.aborted) msg.parts.push({ type: 'error', text: String(e) });
    } finally {
      msg.done = true;
      busy = false;
      controller = null;
      void loadSessions();
    }
  }

  function send() {
    const text = input.trim();
    if (!text || busy) return;
    input = '';
    messages.push({
      id: uid(),
      role: 'user',
      parts: [{ type: 'text', id: 'u', text }],
      steps: 0,
      done: true,
    });
    const msg = store.beginStreaming();
    messages.push(msg);
    void run(msg, '/api/chat', { text });
  }

  const stop = () => controller?.abort();

  function decide(msg: Message, part: ToolPart, approved: boolean) {
    if (!part.approvalId || busy) return;
    part.state = approved ? 'ready' : 'denied';
    void run(store.resumeStreaming(msg), '/api/approve', {
      approvalId: part.approvalId,
      approved,
    });
  }
</script>

<div class="relative h-full overflow-hidden bg-ink" style:--composer-h="{composerH}px">
  <!-- 消息列表铺满整个高度：内容从头部和输入卡片下面穿过去。
       渐隐（mask）在 MessageList 里，所以正文是淡出的，不是盖一层
       背景色 —— 换背景也不会穿帮。 -->
  <main class="absolute inset-0 z-0">
    {#if messages.length === 0}
      <h1 class="grid h-full place-items-center text-2xl font-normal">来思考些什么?</h1>
    {:else}
      <MessageList {messages} ondecide={decide} />
    {/if}
  </main>

  <!--
    渐隐用浮层而不是滚动容器上的 mask。
    mask 会让滚动容器每帧重新合成 —— 实测手机上划动明显卡顿。
    浮层要显式写 z-index：定位元素会盖住非定位元素，光靠 DOM 顺序不够。
  -->

  <!-- 顶部：内容在头部下面淡出 -->
  <div class="fade-top pointer-events-none absolute inset-x-0 top-0 z-10"></div>

  <!-- 底部：内容在输入卡片上方开始淡出，到卡片半腰全遮。
       高度跟着输入区实测高度走 —— 原来写死 h-14，输入框一长高，
       渐隐就整个缩在卡片后面，卡片两侧的正文是硬切的。 -->
  <div class="fade-bottom pointer-events-none absolute inset-x-0 bottom-0 z-10"></div>

  <!-- 头部浮在内容上 -->
  <header
    class="pointer-events-none absolute inset-x-0 top-0 z-20 flex items-center justify-between
           px-4 pb-2 pt-[calc(1.5rem+env(safe-area-inset-top))]"
  >
    <button onclick={openDrawer} aria-label="菜单" class="pointer-events-auto p-1 text-fg">
      <Icon name="menu" size={17} />
    </button>
    <button
      onclick={() => (sheet = true)}
      aria-label="模型设置"
      class="pointer-events-auto size-2 rounded-full {dot}"
      title={store.device?.name ?? '未连接'}
    ></button>
  </header>

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
      {busy}
      onsend={send}
      onstop={stop}
      onmenu={openDrawer}
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
  />
{/if}

{#if sheet}<LlmSheet onclose={close} />{/if}
