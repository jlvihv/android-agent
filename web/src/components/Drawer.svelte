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

  export type Session = { id: string; title: string; active: boolean; count: number };
  /** 搜索命中（GET /api/search） */
  export type Hit = { id: string; title: string; active: boolean; role: string; snippet: string };

  let {
    sessions,
    onclose,
    onnew,
    onswitch,
    ondelete,
    onrename,
  }: {
    sessions: Session[];
    onclose: () => void;
    onnew: () => void;
    onswitch: (id: string) => void;
    ondelete: (id: string) => void;
    onrename: (id: string, title: string) => void;
  } = $props();

  // —— 搜索 ——
  //
  // 输入停顿 180ms 再打后端：一边打字一边发请求纯属浪费。
  // effect 的清理函数会把上一个计时器干掉，所以快速输入不会堆积请求。
  let q = $state('');
  let hits = $state<Hit[]>([]);
  let searching = $state(false);

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

  // 返回键关抽屉
  $effect(() => onBack(() => onclose()));

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
  <input
    bind:value={q}
    type="search"
    placeholder="搜索对话…"
    spellcheck="false"
    autocomplete="off"
    aria-label="搜索对话"
    class="mx-4 mt-4 mb-3 shrink-0 rounded-2xl bg-[#1f1f1f] px-4 py-3 text-base
           text-fg placeholder:text-faint outline-none"
  />

  <div class="min-h-0 flex-1 overflow-y-auto px-3 pb-1">
    {#if q.trim()}
      {#if hits.length === 0}
        <p class="px-3 py-3 text-sm text-faint">
          {searching ? '搜索中…' : '没有匹配的对话'}
        </p>
      {:else}
        {#each hits as h, i (`${h.id}:${i}`)}
          <button
            onclick={() => onswitch(h.id)}
            class="block w-full rounded-2xl px-3 py-2.5 text-left {h.active ? 'bg-[#414141]' : ''}"
          >
            <span class="block truncate text-xs text-faint">{h.title}</span>
            <span class="mt-0.5 block line-clamp-2 text-sm text-fg">{h.snippet}</span>
          </button>
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
            <span class="truncate text-base">{s.title}</span>
          </button>
        {/if}
      {/each}
    {/if}
  </div>

  <div
    class="flex shrink-0 items-center gap-3 px-4 pt-3 pb-[max(0.75rem,env(safe-area-inset-bottom))]"
  >
    <!-- 左下角 = 设置。图标本身说明用途；功能待接（点开还没东西）。
         连接状态当角标挂在这个按钮上，顺手保留了原来那个点的信息。 -->
    <button
      type="button"
      aria-label="设置"
      title="设置"
      class="relative grid size-11 shrink-0 place-items-center rounded-full bg-hairline text-muted"
    >
      <Icon name="gear" size={20} />
      <span
        aria-hidden="true"
        title={store.device?.name ?? '未连接设备'}
        class="absolute right-0.5 bottom-0.5 size-2.5 rounded-full ring-2 ring-[#111]
               {store.device?.connected ? 'bg-accent' : 'bg-faint'}"
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
