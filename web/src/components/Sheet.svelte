<script lang="ts">
  /**
   * 底部弹出面板（覆盖层）。一次只装**一个**东西 —— 一段思考，或一次工具调用。
   *
   * 尺寸按 Claude 的「选择模型」面板实测（1080×2424 @420dpi → 411×923 css）：
   *   全宽贴底，上圆角 **rounded-4xl（32px）**。实测 Claude 是 43px（对整条弧
   *   最小二乘拟合，RMS 残差 0.6px）—— 不追像素对齐，取刻度里最接近的一档
   *   底色 **#151515**（= --color-ink，和页面底色同一个值）—— 面板是靠遮罩把页面
   *   压暗才「浮」起来的，不是自己变亮；里面的卡片才是 #20201f（= --color-surface）
   *   遮罩：页面底色 #151515 被压到 #0e0e0e ≈ 35% 黑
   *   抓手 36×5 居中，距面板顶 5.3
   *   标题 17px 半粗居中，头部下面没有分隔线
   *   左右内边距 **12px**（卡片距屏幕边 12；卡片自己的内边距是 14px）
   *
   * 没有关闭按钮：往下拉、点遮罩、按返回键都能关。
   *
   * 高度：最少 45%（Claude 固定 422/923 ≈ 46%），内容短也不会缩成一条；
   * 最多 75%，再长就在面板内滚。
   */
  import type { Snippet } from 'svelte';
  import { fly } from 'svelte/transition';
  import { cubicOut } from 'svelte/easing';
  import { onBack, type Peek } from '../lib/overlay';
  import { dismissable, RELEASE, PAGE_MS } from '../lib/drag';

  let {
    title,
    onclose,
    onback,
    peek,
    slide = 1,
    minClass = 'min-h-[45dvh]',
    children,
  }: {
    title: string;
    onclose: () => void;
    /** 返回上一层的动作（面板内部换页时给，比如 模型 → 思考强度）。
     *  给了它就接管**系统返回键**：在第二页按返回是退回第一页，而不是把
     *  整个面板关掉。第二页不画返回箭头 —— 安卓本来就有返回键/返回手势，
     *  再画一个纯属重复。 */
    onback?: () => void;
    /** 有 `onback` 时，预测性返回（跟手返回）驱动它，而不是把面板往下拉 ——
     *  回上一页的跟手应该是「上一页从左边滑回来」，不是「面板要关了」。 */
    peek?: Peek;
    /** 标题从哪边飞进来：1 = 从右（内容往左推，进第二页），-1 = 从左（退回）。
     *  只在面板内部换页时有意义。 */
    slide?: number;
    minClass?: string;
    children: Snippet;
  } = $props();

  let panel: HTMLDivElement | null = $state(null);

  /** 0 = 完全展开，1 = 完全收起（在屏幕外） */
  let p = $state(1);
  /** 跟手中：不能有过渡，否则手指和面板之间有延迟 */
  let dragging = $state(false);
  let closing = false;

  // 进场：初始在屏幕外，等首帧画出来再滑进来。
  // 两层 rAF：第一层回调时初始位置还没提交，直接改会被合并成一帧、没有动画。
  $effect(() => {
    let r = requestAnimationFrame(() => {
      r = requestAnimationFrame(() => (p = 0));
    });
    return () => cancelAnimationFrame(r);
  });

  /**
   * 挂到 body 上。
   *
   * 思考/工具面板是在消息列表里渲染的，而消息列表在 `<main class="z-0">`
   * 里 —— 那是个独立的层叠上下文，里面的 z-50 再高也压不过外面 z-20 的
   * 输入卡片，结果输入框叠在面板上面。搬到 body 下就和输入卡片平级比了。
   *
   * 遮罩和面板各挂各的（不再用一个 `display:contents` 的包装元素）：
   * 那个包装只为当 portal 的落点，真正干活的就这两个节点。
   */
  function portal(node: HTMLElement) {
    document.body.appendChild(node);
    return { destroy: () => node.remove() };
  }

  /** 外部想关（比如保存成功）也走动画 */
  export function dismiss() {
    close();
  }

  function close() {
    if (closing) return;
    closing = true;
    dragging = false;
    p = 1;
    setTimeout(onclose, 280);
  }

  /**
   * 返回上一层的动作：第二页 → 退回第一页（面板留在原地），第一页 → 关面板。
   *
   * **返回键和跟手返回必须都走这里。** 之前 `drag.onEnd` 里直接调 `close()`，
   * 而安卓手势返回（预测性返回）commit 的那一下走的正是 `peek.onEnd(true)` ——
   * 于是「在思考强度页返回」直接把整个面板关了，根本轮不到上面这一层。
   *
   * `onback` 是在**调用时**读的，不会被当成依赖；返回值表示面板是否真的要关
   * （`close()` 会把 `p` 推到 1 滑出去，调用方就不要再动 `p` 了）。
   */
  const goBack = (): boolean => {
    if (onback) {
      onback();
      return false;
    }
    close();
    return true;
  };

  /**
   * 交给预测性返回的跟手位移。在第二页（有 `onback` + `peek`）时是页面自己的
   * 横向回退，否则是面板下滑。手势开始那一刻定下来用哪个，整段手势不换；
   * 按返回键（没有 start，直接 commit）就当场挑。
   */
  let active: Peek | null = null;
  const pick = (): Peek => (onback && peek ? peek : drag);
  const backPeek: Peek = {
    onStart: () => (active = pick()).onStart(),
    onMove: (v) => (active ?? pick()).onMove(v),
    onEnd: (commit) => {
      const a = active ?? pick();
      active = null;
      a.onEnd(commit);
    },
  };

  $effect(() => onBack(goBack, backPeek));

  /** 从手指落点往上到面板，有没有哪层已经滚过了（正文、或里面单独滚的清单）。
   *  有的话往下拉是在把它滚回去，不是关面板。 */
  function scrolled(t: EventTarget | null): boolean {
    for (let n = t as HTMLElement | null; n && n !== panel; n = n.parentElement) {
      if (n.scrollTop > 0) return true;
    }
    return false;
  }

  const drag = {
    axis: 'y' as const,
    dir: 1 as const,
    size: () => panel?.offsetHeight ?? 400,
    // 正文（或里面单独滚的清单）没滚到顶时往下拉是在滚它，不接管
    canStart: (t: EventTarget | null) => !closing && !scrolled(t),
    onStart: () => (dragging = true),
    onMove: (v: number) => (p = v),
    onEnd: (shouldClose: boolean) => {
      dragging = false;
      // 真关时 close() 自己会把 p 推到 1；只是回上一层则留在原地
      if (!shouldClose || !goBack()) p = 0;
    },
  };
</script>

<!-- 遮罩。点它即关；拖到哪淡到哪。 -->
<button
  use:portal
  class="fixed inset-0 z-40 bg-black/35"
  style:opacity={1 - p}
  style:transition={dragging ? 'none' : 'opacity 280ms ease-out'}
  aria-label="关闭"
  onclick={close}
></button>

<div
  use:portal
  bind:this={panel}
  use:dismissable={drag}
  role="dialog"
  aria-modal="true"
  aria-label={title}
  tabindex="-1"
  style:transform="translateY({p * 100}%)"
  style:transition={dragging ? 'none' : RELEASE}
  class="fixed inset-x-0 bottom-[var(--kb,0px)] z-50 flex max-h-[75dvh] {minClass} flex-col
         rounded-t-4xl bg-ink pt-1 will-change-transform"
>
  <div class="shrink-0">
    <div class="mx-auto h-1.5 w-9 rounded-full bg-control"></div>
    <!-- 头部高度写死（mt-3 + h-11）：标题绝对定位，
         换页那一瞬新旧标题同时在 DOM 里，不能让它把面板顶开。 -->
    <div class="relative mt-3 h-11">
      <div class="relative h-full overflow-hidden">
        {#key title}
          <h2
            in:fly={{ x: 28 * slide, duration: PAGE_MS, easing: cubicOut }}
            out:fly={{ x: -28 * slide, duration: PAGE_MS, easing: cubicOut }}
            class="absolute inset-0 flex items-center justify-center truncate px-12 text-[17px]
                   font-semibold"
          >
            {title}
          </h2>
        {/key}
      </div>
    </div>
  </div>

  <div
    class="min-h-0 flex-1 overflow-y-auto overscroll-contain px-3 pt-4 pb-[max(1rem,env(safe-area-inset-bottom))] selectable"
  >
    {@render children()}
  </div>
</div>
