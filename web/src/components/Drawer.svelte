<script lang="ts">
  /**
   * 抽屉。结构对齐 Claude 的侧边栏：
   *
   *   agent                              [×]
   *   📱 界面            ← 导航项（Claude 那边是 项目/Code/Artifacts/定时任务）
   *   📋 事件
   *   ────────────────────
   *   最近对话
   *     💬 <标题>        ← 选中项底 #414141
   *   (●)              [+ 新建会话]
   *
   * **长按**对话行 → 弹出菜单（重命名 / 删除）。
   * Claude 就是这样 —— 列表里没有常驻的删除按钮，那太吵。
   *
   * 动画：进出场是 Svelte 过渡（CSS 动画，跑在合成线程上）；
   * 往左拖可以跟手关掉。遮罩也在这里，跟面板一起淡入淡出。
   */
  import { fade } from 'svelte/transition';
  import { cubicIn, quintOut } from 'svelte/easing';
  import { store } from '../lib/store.svelte.ts';
  import { onBack } from '../lib/overlay';
  import Icon from './Icon.svelte';
  import DevicePanel from './DevicePanel.svelte';
  import EventLog from './EventLog.svelte';

  export type Session = { id: string; title: string; active: boolean; count: number };

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

  // null = 显示列表；否则显示对应面板
  let panel = $state<'device' | 'events' | null>(null);

  // 返回键：先退子面板，再关抽屉
  $effect(() => onBack(() => onclose()));
  $effect(() => {
    if (panel) return onBack(() => (panel = null));
  });

  // —— 拖拽关闭 ——
  //
  // aside 上是 touch-action: pan-y：竖直方向交给浏览器滚列表（会收到
  // pointercancel），水平方向的 pointermove 才归我们。
  let aside: HTMLElement | null = $state(null);
  /** 被往左拖出去的比例，0 = 完全展开，1 = 完全收起 */
  let off = $state(0);
  /** 跟手中：关掉回弹过渡，否则拖起来发黏 */
  let dragging = $state(false);
  let drag: { id: number; x0: number; y0: number; t0: number; w: number; on: boolean } | null =
    null;

  function dragStart(e: PointerEvent) {
    if (e.pointerType === 'mouse' && e.button !== 0) return;
    drag = { id: e.pointerId, x0: e.clientX, y0: e.clientY, t0: e.timeStamp, w: aside?.offsetWidth ?? 1, on: false };
  }

  function dragMove(e: PointerEvent) {
    if (!drag || e.pointerId !== drag.id) return;
    const dx = e.clientX - drag.x0;
    if (!drag.on) {
      // 先认方向：水平位移明显大于竖直才算拖抽屉
      if (Math.abs(dx) < 8 || Math.abs(dx) < Math.abs(e.clientY - drag.y0)) return;
      drag.on = true;
      dragging = true;
      clearTimeout(pressTimer);
      aside?.setPointerCapture(e.pointerId);
    }
    off = Math.min(1, Math.max(0, -dx / drag.w));
  }

  function dragEnd(e: PointerEvent) {
    if (!drag || e.pointerId !== drag.id) return;
    const d = drag;
    drag = null;
    if (!d.on) return;
    dragging = false;
    // 拖完别把这次抬手当成点击
    longPressed = true;
    const v = off / Math.max(1, e.timeStamp - d.t0); // 每毫秒收起的比例
    if (off > 0.35 || (v > 0.0015 && off > 0.08)) onclose();
    else off = 0;
  }

  /** 进场：从左边整个滑进来 */
  function slideIn(_: Element) {
    return {
      duration: 320,
      easing: quintOut,
      css: (_t: number, u: number) => `transform: translateX(${-u * 100}%)`,
    };
  }

  /** 出场：从当前位置（可能被拖到一半）继续往左收 */
  function slideOut(_: Element) {
    const from = off;
    return {
      duration: 220 * (1 - from) + 60,
      easing: cubicIn,
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
  class="fixed inset-0 z-40 bg-black/50 {dragging ? '' : 'transition-opacity duration-200'}"
  style:opacity={1 - off}
  aria-label="关闭"
  onclick={onclose}
  transition:fade|global={{ duration: 220 }}
></button>

<aside
  bind:this={aside}
  onpointerdown={dragStart}
  onpointermove={dragMove}
  onpointerup={dragEnd}
  onpointercancel={dragEnd}
  style:transform={off ? `translateX(${-off * 100}%)` : null}
  class="fixed inset-y-0 left-0 z-50 flex w-[92%] touch-pan-y flex-col bg-[#111]
         pt-[env(safe-area-inset-top)] {dragging ? '' : 'transition-transform duration-200 ease-out'}"
  in:slideIn|global
  out:slideOut|global
>
  {#if panel === null}
    <div class="flex shrink-0 items-center px-5 pt-6 pb-4">
      <span class="text-2xl font-semibold tracking-tight">agent</span>
      <span class="ml-2 self-end pb-1 font-mono text-meta text-faint"
        >v{store.hello?.version ?? '…'}</span
      >
      <button class="ml-auto p-1 text-muted" aria-label="关闭" onclick={onclose}>
        <Icon name="close" size={20} />
      </button>
    </div>

    <nav class="flex shrink-0 flex-col">
      {#each [['device', '界面', 'screen'], ['events', '事件', 'list']] as const as [k, label, icon] (k)}
        <button
          onclick={() => (panel = k)}
          class="flex items-center gap-3.5 px-5 py-3.5 text-base text-fg"
        >
          <Icon name={icon} size={20} class="shrink-0 text-muted" />
          {label}
        </button>
      {/each}
    </nav>

    <div class="mx-5 my-2 shrink-0 border-t border-hairline"></div>

    <div class="shrink-0 px-5 pb-2 text-xs text-faint">最近对话</div>

    <div class="min-h-0 flex-1 overflow-y-auto px-3">
      {#if sessions.length === 0}
        <p class="px-2 py-3 text-sm text-faint">还没有对话</p>
      {/if}
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
    </div>

    <div
      class="flex shrink-0 items-center gap-3 px-4 pt-3 pb-[max(0.75rem,env(safe-area-inset-bottom))]"
    >
      <span
        class="grid size-11 shrink-0 place-items-center rounded-full bg-hairline"
        title={store.device?.name ?? '未连接设备'}
      >
        <span class="size-2.5 rounded-full {store.device?.connected ? 'bg-accent' : 'bg-faint'}"
        ></span>
      </span>

      <button
        onclick={onnew}
        class="ml-auto flex h-11 items-center gap-1.5 rounded-full bg-fg px-5 text-sm font-medium text-ink"
      >
        <Icon name="plus" size={16} />
        新建会话
      </button>
    </div>
  {:else}
    <div class="flex shrink-0 items-center gap-2 px-4 pt-5 pb-3">
      <button class="p-1 text-fg" aria-label="返回" onclick={() => (panel = null)}>
        <Icon name="back" size={20} />
      </button>
      <span class="text-base font-medium">{panel === 'device' ? '界面' : '事件'}</span>
      <button class="ml-auto p-1 text-muted" aria-label="关闭" onclick={onclose}>
        <Icon name="close" size={20} />
      </button>
    </div>
    <div class="min-h-0 flex-1 border-t border-hairline">
      {#if panel === 'device'}<DevicePanel />{:else}<EventLog />{/if}
    </div>
  {/if}
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
