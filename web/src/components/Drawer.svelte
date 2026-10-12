<script lang="ts">
  /**
   * 抽屉（侧边栏）。
   *
   *   [ 搜索对话… ]                  ← 顶上就一个搜索框，输入即搜（跨会话搜正文）
   *   💬 新对话                     ← 没搜 = 最近对话；搜了 = 命中（标题 + 摘要）
   *   💬 帮我看看屏幕上是什么界面
   *   (⚙)              [+ 新建会话]
   *
   * 去掉了：agent 标题栏 + 版本号、事件入口、分割线、「最近对话」标题。
   * 搜索框替掉标题栏：边栏本来就是用来找对话的，搜索比版本号有用。
   * 事件日志（EventLog）以后挂进「设置」里。
   *
   * **长按**对话行 → 重命名 / 删除。跟 Claude 一样，列表里不放常驻按钮。
   *
   * DOM 刻意浅：aside 只有三个孩子（搜索框 / 列表 / 底栏），
   * 中间那层仅仅是滚动容器 —— 滚动区必须是单个元素，省不掉。
   *
   * 动画：进出场是 Svelte 过渡（CSS 动画，跑在合成线程上）；
   * 往左拖可以跟手关掉。遮罩也在这里，跟面板一起淡入淡出。
   * 没有关闭按钮：往左拖、点遮罩、按返回键都能关。
   */
  import { fade } from 'svelte/transition';
  import { quintOut } from 'svelte/easing';
  import { store } from '../lib/store.svelte.ts';
  import { onBack } from '../lib/overlay';
  import { dismissable, RELEASE } from '../lib/drag';
  import Icon from './Icon.svelte';

  export type Session = {
    id: string;
    title: string;
    active: boolean;
    count: number;
    /** 这个会话有回合在跑（可以同时在几个会话里各跑一个） */
    busy?: boolean;
  };
  /** 搜索命中（GET /api/search） */
  export type Hit = { id: string; title: string; active: boolean; role: string; snippet: string };

  let {
    sessions,
    onclose,
    onnew,
    onswitch,
    ondelete,
    onrename,
    onsettings,
  }: {
    sessions: Session[];
    onclose: () => void;
    onnew: () => void;
    onswitch: (id: string) => void;
    ondelete: (id: string) => void;
    onrename: (id: string, title: string) => void;
    /** 左下角的齿轮 → 设置页 */
    onsettings: () => void;
  } = $props();

  // —— 搜索 ——
  //
  // 输入停顿 180ms 再打后端：一边打字一边发请求纯属浪费。
  // effect 的清理函数会把上一个计时器干掉，所以快速输入不会堆积请求。
  let q = $state('');
  let hits = $state<Hit[]>([]);
  let searching = $state(false);

  /**
   * 命中按会话分组。
   *
   * 以前是平铺的，同一个会话的三条命中会把标题重复三遍，而且
   * `h.active`（当前打开的那个会话）会给其中几条涂上 #414141 ——
   * 看着像“这几条被选中了”，其实跟搜索无关。
   */
  const groups = $derived.by(() => {
    const m = new Map<string, { id: string; title: string; hits: Hit[] }>();
    for (const h of hits) {
      let g = m.get(h.id);
      if (!g) m.set(h.id, (g = { id: h.id, title: h.title, hits: [] }));
      g.hits.push(h);
    }
    return [...m.values()];
  });

  /** 摘要里全是 markdown 记号，直接显示很脏（`**1. \`bash\`**`）。去记号留字。
      只去强调和反引号，以及最前面那个列表项标记 —— 正文里的 `1)` `-` 是内容。 */
  function plain(s: string): string {
    return s
      .replace(/\*\*|__|`{1,3}/g, '')
      .replace(/^\s*(#+|\d+[.)]|[-*+])\s+/, '')
      .replace(/\s+/g, ' ')
      .trim();
  }

  /** 把摘要按命中词切成段，命中那段提亮 —— 不然看不出为什么它被搜出来 */
  function mark(text: string, needle: string): { t: string; hit: boolean }[] {
    const n = needle.trim();
    if (!n) return [{ t: text, hit: false }];
    const out: { t: string; hit: boolean }[] = [];
    const lower = text.toLowerCase();
    const nl = n.toLowerCase();
    let i = 0;
    for (;;) {
      const at = lower.indexOf(nl, i);
      if (at < 0) break;
      if (at > i) out.push({ t: text.slice(i, at), hit: false });
      out.push({ t: text.slice(at, at + n.length), hit: true });
      i = at + n.length;
    }
    if (i < text.length) out.push({ t: text.slice(i), hit: false });
    return out;
  }

  $effect(() => {
    const needle = q.trim();
    if (!needle) {
      hits = [];
      searching = false;
      return;
    }
    searching = true;
    const t = setTimeout(() => void search(needle), 180);
    return () => clearTimeout(t);
  });

  async function search(needle: string) {
    try {
      const r = await fetch(`/api/search?q=${encodeURIComponent(needle)}`);
      if (r.ok) hits = ((await r.json()) as { hits: Hit[] }).hits ?? [];
    } catch {
      /* 后端不在就当没搜到 */
    } finally {
      searching = false;
    }
  }

  // —— 拖拽关闭 ——
  //
  // 按住抽屉任何地方往左拖都跟手（包括会话列表上）；竖着划还是滚列表。
  // 手势判定见 lib/drag.ts。
  let aside: HTMLElement | null = $state(null);
  /** 被往左拖出去的比例，0 = 完全展开，1 = 完全收起 */
  let off = $state(0);
  /** 跟手中：关掉回弹过渡，否则拖起来发黏 */
  let dragging = $state(false);

  const drag = {
    axis: 'x' as const,
    dir: -1 as const,
    size: () => aside?.offsetWidth ?? 1,
    onStart: () => {
      dragging = true;
      clearTimeout(pressTimer);
    },
    onMove: (v: number) => (off = v),
    onEnd: (close: boolean) => {
      dragging = false;
      if (close) onclose();
      else off = 0;
    },
  };

  // 返回键关抽屉；拖拽三件套也是「预测性返回」的跟手位移
  // （系统手势划到哪，抽屉就跟着挪到哪。Android 14+；桌面可手动 __predictiveBack 模拟）
  $effect(() => onBack(() => onclose(), drag));

  /** 进场：从左边整个滑进来 */
  function slideIn(_: Element) {
    return {
      duration: 320,
      easing: quintOut,
      css: (_t: number, u: number) => `transform: translateX(${-u * 100}%)`,
    };
  }

  /** 出场：从当前位置（可能被拖到一半）继续往左收。
   *  先快后慢 —— 拖完松手时手指本来就在动，减速收尾才接得住。 */
  function slideOut(_: Element) {
    const from = off;
    return {
      duration: 240 * (1 - from) + 80,
      easing: quintOut,
      css: (_t: number, u: number) => `transform: translateX(${-(from + (1 - from) * u) * 100}%)`,
    };
  }

  // 长按弹出的菜单
  let menu = $state<{ id: string; x: number; y: number } | null>(null);
  // 正在重命名的行
  let renaming = $state<string | null>(null);
  let draft = $state('');

  let pressTimer: ReturnType<typeof setTimeout> | undefined;
  let longPressed = false;

  function pressStart(e: PointerEvent, id: string) {
    longPressed = false;
    clearTimeout(pressTimer);
    const { clientX: x, clientY: y } = e;
    pressTimer = setTimeout(() => {
      longPressed = true;
      menu = { id, x, y };
      navigator.vibrate?.(12);
    }, 420);
  }
  const pressEnd = () => clearTimeout(pressTimer);

  function pick(id: string) {
    const s = sessions.find((v) => v.id === id);
    draft = s?.title ?? '';
    renaming = id;
    menu = null;
  }

  /** autofocus 会触发 a11y 警告，用 action 自己聚焦 */
  function focusOnMount(node: HTMLInputElement) {
    queueMicrotask(() => {
      node.focus();
      node.select();
    });
  }

  function commitRename() {
    if (renaming) onrename(renaming, draft.trim() || '新对话');
    renaming = null;
  }
</script>

<button
  class="fixed inset-0 z-40 bg-black/50"
  style:opacity={1 - off}
  style:transition={dragging ? 'none' : 'opacity 280ms ease-out'}
  aria-label="关闭"
  onclick={onclose}
  transition:fade|global={{ duration: 220 }}
></button>

<aside
  bind:this={aside}
  use:dismissable={drag}
  style:transform={off ? `translateX(${-off * 100}%)` : null}
  style:transition={dragging ? 'none' : RELEASE}
  class="fixed top-0 bottom-[var(--kb,0px)] left-0 z-50 flex w-[92%] flex-col bg-[#111]
         pt-[env(safe-area-inset-top)] will-change-transform"
  in:slideIn|global
  out:slideOut|global
>
  <!-- 全圆角 + 里面一个放大镜。放大镜是 CSS 背景图（不套 div/span）：
       纯装饰，为它多一层 DOM 不值当。图形跟 Icon.svelte 一个规格
       （20 画布 / 描边 1.8），颜色取 muted。 -->
  <input
    bind:value={q}
    type="search"
    placeholder="搜索对话…"
    spellcheck="false"
    autocomplete="off"
    aria-label="搜索对话"
    class="search-input mx-4 mt-1 mb-3 shrink-0 rounded-full bg-[#1f1f1f] py-3 pr-4 pl-10 text-base
           text-fg placeholder:text-faint outline-none"
  />

  <div class="min-h-0 flex-1 overflow-y-auto px-3 pt-1 pb-1">
    {#if q.trim()}
      {#if hits.length === 0}
        <p class="px-3 py-3 text-sm text-faint">
          {searching ? '搜索中…' : '没有匹配的对话'}
        </p>
      {:else}
        {#each groups as g (g.id)}
          <!-- 会话名只在组头出现一次（图标跟会话列表一致） -->
          <p class="flex items-center gap-2 px-3 pt-3 pb-1 text-xs text-faint">
            <Icon name="chat" size={13} class="shrink-0 text-muted" />
            <span class="truncate">{g.title}</span>
          </p>
          {#each g.hits as h, i (`${g.id}:${i}`)}
            <button
              onclick={() => onswitch(g.id)}
              class="block w-full rounded-2xl px-3 py-2 text-left active:bg-[#2a2a28]"
            >
              <span class="line-clamp-2 text-sm"
                ><span class="mr-1.5 text-meta text-faint"
                  >{h.role === 'user' ? '你' : 'agent'}</span
                >{#each mark(plain(h.snippet), q) as p}{#if p.hit}<mark
                      class="bg-transparent font-medium text-fg">{p.t}</mark
                    >{:else}<span class="text-muted">{p.t}</span>{/if}{/each}</span
              >
            </button>
          {/each}
        {/each}
      {/if}
    {:else if sessions.length === 0}
      <p class="px-3 py-3 text-sm text-faint">还没有对话</p>
    {:else}
      {#each sessions as s (s.id)}
        {#if renaming === s.id}
          <input
            bind:value={draft}
            use:focusOnMount
            onblur={commitRename}
            onkeydown={(e) => e.key === 'Enter' && commitRename()}
            class="my-1 w-full rounded-2xl bg-[#414141] px-3 py-3 text-base text-fg outline-none"
          />
        {:else}
          <button
            onpointerdown={(e) => pressStart(e, s.id)}
            onpointerup={pressEnd}
            onpointercancel={pressEnd}
            oncontextmenu={(e) => e.preventDefault()}
            onclick={() => {
              if (longPressed) return;
              onswitch(s.id);
            }}
            class="flex w-full items-center gap-3 rounded-2xl px-3 py-3 text-left
                   {s.active ? 'bg-[#414141]' : ''}"
          >
            <Icon name="chat" size={18} class="shrink-0 text-muted" />
            <span class="min-w-0 flex-1 truncate text-base">{s.title}</span>
            {#if s.busy}
              <!-- 还在跑：切走了也看得出来 -->
              <span class="size-1.5 shrink-0 animate-pulse rounded-full bg-warn" aria-label="运行中"></span>
            {/if}
          </button>
        {/if}
      {/each}
    {/if}
  </div>

  <div
    class="flex shrink-0 items-center gap-3 px-4 pt-3 pb-[max(0.75rem,env(safe-area-inset-bottom))]"
  >
    <!-- 左下角 = 设置。图标本身说明用途。
         连接状态当角标挂在这个按钮上，顺手保留了原来那个点的信息。 -->
    <button
      type="button"
      onclick={onsettings}
      aria-label="设置"
      title="设置"
      class="relative grid size-11 shrink-0 place-items-center rounded-full bg-hairline text-muted"
    >
      <Icon name="gear" size={20} />
      <span
        aria-hidden="true"
        title={store.device?.name ?? '未连接设备'}
        class="absolute right-0.5 bottom-0.5 size-2.5 rounded-full ring-2 ring-[#111]
               {store.device?.connected ? 'bg-ok' : 'bg-faint'}"
      ></span>
    </button>

    <button
      onclick={onnew}
      class="ml-auto flex h-11 items-center gap-1.5 rounded-full bg-fg px-5 text-sm font-medium text-ink"
    >
      <Icon name="plus" size={16} />
      新建会话
    </button>
  </div>
</aside>

{#if menu}
  {@const m = menu}
  <!-- 长按菜单。点别处关掉。 -->
  <button class="fixed inset-0 z-[60]" aria-label="关闭菜单" onclick={() => (menu = null)}></button>
  <div
    class="fixed z-[61] w-40 overflow-hidden rounded-2xl bg-[#2a2a28] shadow-2xl shadow-black/60"
    style="left: min({m.x}px, calc(92vw - 10.5rem)); top: min({m.y}px, calc(100dvh - 8.5rem))"
  >
    <button
      onclick={() => pick(m.id)}
      class="flex w-full items-center gap-3 px-4 py-3.5 text-left text-base text-fg"
    >
      <Icon name="pencil" size={18} class="text-muted" />
      重命名
    </button>
    <button
      onclick={() => {
        ondelete(m.id);
        menu = null;
      }}
      class="flex w-full items-center gap-3 px-4 py-3.5 text-left text-base text-[#da6262]"
    >
      <Icon name="trash" size={18} />
      删除
    </button>
  </div>
{/if}
