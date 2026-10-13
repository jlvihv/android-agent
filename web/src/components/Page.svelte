<script lang="ts">
  /**
   * 全屏页面：从右边滑进来，盖住对话页。设置页、服务商详情页都是它。
   *
   * 和查看器（Viewer）是同一套手感：
   *   - 左上角返回箭头、返回键、跟手返回（预测性返回）都能关
   *   - 按住往右拖也能关（竖着划还是滚内容，判定见 lib/drag.ts）
   *
   * 几个页面叠着开（设置 → 某个服务商）时，每页各自在 lib/overlay.ts 登记，
   * 返回键先关最上面那一页。
   */
  import type { Snippet } from 'svelte';
  import { quintOut } from 'svelte/easing';
  import Icon from './Icon.svelte';
  import { onBack } from '../lib/overlay';
  import { dismissable, RELEASE } from '../lib/drag';

  let {
    title,
    onclose,
    children,
  }: {
    title: string;
    onclose: () => void;
    children: Snippet;
  } = $props();

  let box: HTMLElement | null = $state(null);
  /** 被往右拖出去的比例，0 = 原位，1 = 完全移出屏幕 */
  let off = $state(0);
  let dragging = $state(false);

  /** 外部想关（比如保存成功）也走同一个出场动画 */
  export function dismiss() {
    onclose();
  }

  const drag = {
    axis: 'x' as const,
    dir: 1 as const,
    size: () => box?.offsetWidth ?? 1,
    onStart: () => (dragging = true),
    onMove: (v: number) => (off = v),
    onEnd: (close: boolean) => {
      dragging = false;
      if (close) onclose();
      else off = 0;
    },
  };

  $effect(() => onBack(() => onclose(), drag));

  function slideIn(_: Element) {
    return {
      duration: 300,
      easing: quintOut,
      css: (_t: number, u: number) => `transform: translateX(${u * 100}%)`,
    };
  }

  /** 输入框拿到焦点：等键盘弹完（`--kb` 更新、底部留白撑开）再滚到中间 */
  function revealFocused(e: FocusEvent) {
    const t = e.target as HTMLElement | null;
    if (!t || !t.matches('input, textarea')) return;
    setTimeout(() => t.scrollIntoView({ block: 'center', behavior: 'smooth' }), 350);
  }

  /** 从当前拖到的位置接着滑出去，不跳回原位再走 */
  function slideOut(_: Element) {
    const from = off;
    return {
      duration: 240 * (1 - from) + 80,
      easing: quintOut,
      css: (_t: number, u: number) => `transform: translateX(${(from + (1 - from) * u) * 100}%)`,
    };
  }
</script>

<section
  bind:this={box}
  use:dismissable={drag}
  class="fixed inset-0 z-[60] flex flex-col bg-ink will-change-transform"
  style:transform={off ? `translateX(${off * 100}%)` : null}
  style:transition={dragging ? 'none' : RELEASE}
  in:slideIn|global
  out:slideOut|global
  aria-label={title}
>
  <!-- 顶栏高度和对话页 / 查看器一样（`--hdr`，含安全区；安全区当内边距扣掉） -->
  <header class="flex h-[var(--hdr)] shrink-0 items-center gap-2 px-3 pt-[env(safe-area-inset-top)]">
    <button class="p-1 text-fg" aria-label="返回" onclick={onclose}>
      <Icon name="back" size={20} />
    </button>
    <span class="min-w-0 truncate text-base font-medium">{title}</span>
  </header>

  <!-- 键盘：页面不跟着缩（见 main.ts，键盘浮在上面、高度在 `--kb`），所以底部
       留出键盘那么高，再把聚焦的输入框滚到中间 —— 不然靠下的输入框（「连接电脑」
       的地址、服务商的 Key）连同它的按钮都被键盘盖住（真机上踩到）。 -->
  <div
    class="min-h-0 flex-1 overflow-y-auto overscroll-contain px-4 pt-2 pb-[max(1.5rem,env(safe-area-inset-bottom),calc(var(--kb,0px)+1.5rem))]"
    onfocusin={revealFocused}
  >
    {@render children()}
  </div>
</section>
