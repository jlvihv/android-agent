<script lang="ts">
  /**
   * 底部弹出面板（覆盖层）。一次只装**一个**东西 —— 一段思考，或一次工具调用。
   *
   * 尺寸照 Claude 的「选择模型」面板（1080×2424 @420dpi → 411×923 css）：
   *   全宽贴底，上圆角 **rounded-4xl（32px）**。Claude 那边是 43px（对整条弧
   *   最小二乘拟合，RMS 残差 0.6px）—— 不追像素对齐，取刻度里最接近的一档
   *   底色 **#111111**（= --color-panel）—— 页面是纯黑（= --color-ink），遮罩又
   *   压不暗纯黑，面板只能**自己比页面亮一档**才浮得起来（以前靠遮罩把 #151515 的
   *   页面压下去）；里面的卡片才是 #20201f（= --color-surface）
   *   遮罩：页面上的内容（文字 / 图片）被压暗 ≈ 35% 黑
   *   抓手 36×5 居中，距面板顶 5.3
   *   标题 17px 半粗居中，头部下面没有分隔线
   *   左右内边距 **12px**（卡片距屏幕边 12；卡片自己的内边距是 14px）
   *
   * 没有关闭按钮：往下拉、点遮罩、按返回键都能关。
   *
   * 高度：最少 45%、最多 75%（`--sheet-min-h` / `--sheet-max-h`，写在一处），
   * 内容短也不会缩成一条；再长就在面板内滚。
   * “就几行字”的面板（附件 / 用量 / 模型）传 `hug`，贴合内容。
   */
  import type { Snippet } from "svelte";
  import { fly } from "svelte/transition";
  import { cubicOut } from "svelte/easing";
  import { onBack, type Peek } from "../lib/overlay";
  import { dismissable, RELEASE, PAGE_MS } from "../lib/drag";

  /** 滑出 / 淡出遮罩的时长。面板的 transform 和遮罩的 opacity 必须同步，
   *  `close()` 那个 setTimeout 也用它（等动画跑完再卸载）。 */
  const CLOSE_MS = 280;
  /** 换页时标题飞入 / 飞出的横向位移（px） */
  const PAGE_SHIFT = 28;
  /** 面板还没量出高度时的兜底（只在首帧就跟手时可能用到） */
  const FALLBACK_H = 400;

  let {
    title,
    onclose,
    onback,
    peek,
    slide = 1,
    /** 贴合内容（`min-h-0`）：就几行数字的面板别摚到 45% 屏高。
     *  不传就是默认下限（正文类的思考/工具输出用）。 */
    hug = false,
    padClass = "px-3",
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
    /** 高度贴合内容（不摚到 `--sheet-min-h`） */
    hug?: boolean;
    /** 内容区的左右内边距，默认 `px-3`。
     *
     * 横向翻页的面板（「选择模型」）要传 `px-0`：它的内容是一整条 `w-[200%]`
     * 轨道，边距挂在内容区上的话**两页交界处没有边距** —— 滑到中间时左页的
     * 卡片和右页的卡片贴成一条（x=12..399 连续无缝）。那两页自己带
     * `px-3`，交界处才有 24px 的面板色。 */
    padClass?: string;
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
   * 遮罩和面板各挂各的：真正干活的就这两个节点（`display: contents` 的包装
   * 只为当 portal 的落点）。
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
    setTimeout(onclose, CLOSE_MS);
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
    for (
      let n = t as HTMLElement | null;
      n && n !== panel;
      n = n.parentElement
    ) {
      if (n.scrollTop > 0) return true;
    }
    return false;
  }

  const drag = {
    axis: "y" as const,
    dir: 1 as const,
    size: () => panel?.offsetHeight ?? FALLBACK_H,
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

<!-- 遮罩。点它即关；拖到哪淡到哪。

     层级：遮罩 55、面板 56 —— 都在侧栏（50）**上面**。「选择机器」是从侧栏底栏点开的，
     侧栏还开着；遮罩要是在侧栏下面，点面板上方那块想关面板，点中的是侧栏里的会话记录。
     整页的覆盖层（设置页 60、查看器 70）照旧在面板上面：模型面板里能进设置页。 -->
<button
  use:portal
  class="fixed inset-0 z-[55] bg-scrim"
  style:opacity={1 - p}
  style:transition={dragging ? "none" : `opacity ${CLOSE_MS}ms ease-out`}
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
  style:transition={dragging ? "none" : RELEASE}
  class="fixed inset-x-0 bottom-[var(--kb,0px)] z-[56] flex max-h-[var(--sheet-max-h)] {hug
    ? 'min-h-0'
    : 'min-h-[var(--sheet-min-h)]'} flex-col
         rounded-t-4xl bg-panel pt-1 will-change-transform"
>
  <div class="shrink-0">
    <div class="mx-auto h-1.5 w-9 rounded-full bg-control"></div>
    <!-- 头部高度写死（mt-3 + h-11）：标题绝对定位，
         换页那一瞬新旧标题同时在 DOM 里，不能让它把面板顶开。 -->
    <div class="relative mt-3 h-11">
      <div class="relative h-full overflow-hidden">
        {#key title}
          <h2
            in:fly={{
              x: PAGE_SHIFT * slide,
              duration: PAGE_MS,
              easing: cubicOut,
            }}
            out:fly={{
              x: -PAGE_SHIFT * slide,
              duration: PAGE_MS,
              easing: cubicOut,
            }}
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
    class="min-h-0 flex-1 overflow-y-auto overscroll-contain {padClass} pt-4 pb-[max(1rem,env(safe-area-inset-bottom))] selectable"
  >
    {@render children()}
  </div>
</div>
