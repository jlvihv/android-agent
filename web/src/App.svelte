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
  let messages = $state<Message[]>([]);
  let sessions = $state<Session[]>([]);
  let controller: AbortController | null = null;

  let drawer = $state(false);
  let sheet = $state(false);

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
      if (r.ok) messages = ((await r.json()) as { messages: Message[] }).messages ?? [];
    } catch {
      /* 拉不到就算了 */
    }
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

  // 让 Android 的返回键能关掉覆盖层。
  //
  // 抽屉/面板是 JS 覆盖层，系统的返回键不知道它们存在 —— 不处理的话
  // 按返回会直接退出 App（实测踩过）。做法：打开时往 history 里 push
  // 一个状态，返回键触发 popstate 时关闭。**所有关闭都走 history**，
  // 否则会留下脏记录，下次返回要按两下。
  let depth = 0;
  $effect(() => {
    if (drawer || sheet) window.history.pushState({ overlay: ++depth }, '');
  });
  $effect(() => {
    const onPop = () => {
      if (drawer || sheet) {
        drawer = false;
        sheet = false;
      }
    };
    addEventListener('popstate', onPop);
    return () => removeEventListener('popstate', onPop);
  });

  const close = () => {
    if (drawer || sheet) window.history.back();
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

<div class="flex h-full flex-col bg-ink">
  <header
    class="flex shrink-0 items-center justify-between px-4 pb-2 pt-[calc(1.5rem+env(safe-area-inset-top))]"
  >
    <button onclick={openDrawer} aria-label="菜单" class="p-1 text-fg">
      <Icon name="menu" size={17} />
    </button>
    <button
      onclick={() => (sheet = true)}
      aria-label="模型设置"
      class="size-2 rounded-full {dot}"
      title={store.device?.name ?? '未连接'}
    ></button>
  </header>

  <main class="flex min-h-0 flex-1 flex-col">
    {#if messages.length === 0}
      <h1 class="grid flex-1 place-items-center text-2xl font-normal">来思考些什么?</h1>
    {:else}
      <MessageList {messages} ondecide={decide} />
    {/if}
  </main>

  <Composer
    bind:value={input}
    {busy}
    onsend={send}
    onstop={stop}
    onmenu={openDrawer}
    onmodel={() => (sheet = true)}
  />
</div>

{#if drawer}
  <button class="fixed inset-0 z-40 bg-black/50" aria-label="关闭" onclick={close}></button>
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
