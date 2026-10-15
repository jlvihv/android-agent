<script lang="ts">
  /**
   * 上下文用量：一个圆环 + 点开的面板。放在输入卡片里、模型药丸的右边。
   *
   * 环里那个数是**当前上下文占模型上限的百分比** —— 也就是「这次请求发出去的
   * token / 这个模型的窗口」。分母来自 models.dev（`/api/models` 的
   * `context_window`）；没收录的模型就只有用量、没有百分比，宁可空着也不编一个
   * 假上限（环就只剩一条轨道）。
   *
   * 数字口径（面板里都写清楚，不让人猜）：
   *   上下文   = 最近一次请求的 `input`（整段历史 + 系统提示词 + 工具）
   *   面板里的桶 = 本会话累计 —— 未缓存输入 / 缓存读取 / 缓存写入 / 输出，
   *              字段集和 DeepSeek Harness 的会话用量面板一致；命中率在核心
   *              算好（见 `core/src/openai.rs` 的 `UsageView`），前端只读不重算。
   *              数字用 k / M 这种给人看的格式。
   */
  import { type SessionUsage } from '../lib/sse';
  import Sheet from './Sheet.svelte';

  let {
    usage,
    ctxWindow,
  }: {
    /** 本会话用量；一次 LLM 调用都没跑过就是 null */
    usage: SessionUsage | null;
    /** 模型上下文上限（token）。不知道就是 null */
    ctxWindow: number | null;
  } = $props();

  let open = $state(false);

  /**
   * 两个环的几何（`size` 外框 / `r` 半径 / `sw` 线宽 / `fs` 数字），单位都是 px。
   * 小的那个外框 36 = 输入卡片里其他圆钮（「+」、发送）的直径，同一行才对称；
   * 线宽 3 和 Icon.svelte 的描边比例（20 画布 / 1.8）对齐。大的给面板用。
   */
  const RING = { size: 36, r: 15.4, sw: 3, fs: 12 };
  const RING_BIG = { size: 56, r: 22, sw: 5.2, fs: 18 };
  /** 环里那个数低于这个百分比就写 `<1`（0 看着像坏了） */
  const TINY_PCT = 0.5;
  /** `fmt` 的进位门槛 */
  const KILO = 1_000;
  const MEGA = 1_000_000;

  const used = $derived(usage?.last.input ?? 0);
  /** 0..1；没有窗口就是 null */
  const ratio = $derived(ctxWindow ? Math.min(1, used / ctxWindow) : null);
  /**
   * 环里那两个字。四舍五入到整数，但**明明有量却不到 0.5% 时不写 0** ——
   * 1M 窗口的头几轮就是这样（第一句 prompt 才 1468），写个 0 看着像坏了。
   */
  const ringText = $derived(
    ratio == null
      ? null
      : ratio > 0 && ratio * 100 < TINY_PCT
        ? '<1'
        : String(Math.round(ratio * 100)),
  );
  /** 面板和 tooltip 用的百分比：一位小数（环里那个数是整数 —— 那地方小，塞不下） */
  const pctLabel = $derived(ratio == null ? null : `${(ratio * 100).toFixed(1)}%`);



  // —— 数字排版 ——
  //
  // token 数只保留三位有效：`1.2M` / `36.2k` / `940`。面板是给人瞟一眼的，
  // 精确到个位只是噪声（真实值在 /api/session 里，随时能看）。
  function fmt(n: number): string {
    // 过万就不给小数位了：`12.3k` 三位有效数字够看，`12345` 精确到个位只是噪声
    if (n >= MEGA) return `${(n / MEGA).toFixed(n >= MEGA * 10 ? 0 : 1)}M`;
    if (n >= KILO) return `${(n / KILO).toFixed(n >= KILO * 10 ? 0 : 1)}k`;
    return String(n);
  }
  const fmtPct = (r: number) => `${(r * 100).toFixed(1)}%`;

</script>

<!--
  环。两处用同一段：输入框里那个小的、面板里那个大的 —— 只是尺寸不同，
  几何按 size 现算，别维护两份。

  `size` 是外框像素；`r`/`sw` 是环半径和线宽（同一套坐标）。输入框那个**环本身
  就画满 36**（和「+」那个圆钮一样大），**不套一层底色圆** —— 它是读数，不是按钮
  底色；点它的热区也是这 36。
-->
{#snippet ring(size: number, r: number, sw: number, fs: number)}
  {@const c = 2 * Math.PI * r}
  {@const half = size / 2}
  <svg width={size} height={size} viewBox="0 0 {size} {size}" fill="none" class="text-muted">
    <!-- 轨道 -->
    <circle cx={half} cy={half} {r} stroke="currentColor" stroke-width={sw} class="text-muted/30" />
    <!-- 用量。没有上限时只有轨道（不给假信息） -->
    {#if ratio != null}
      <circle
        cx={half}
        cy={half}
        {r}
        stroke="currentColor"
        stroke-width={sw}
        stroke-linecap="round"
        stroke-dasharray={c}
        stroke-dashoffset={c * (1 - (ratio ?? 0))}
        transform="rotate(-90 {half} {half})"
      />
    {/if}
    {#if ringText != null}
      <text
        x={half}
        y={half}
        text-anchor="middle"
        dominant-baseline="central"
        class="fill-current text-fg"
        font-size={fs}
        style="font-variant-numeric: tabular-nums"
      >
        {ringText}
      </text>
    {/if}
  </svg>
{/snippet}

<!-- 环本身就是按钮。没有数据时**不占位** —— 输入框那一行本来就不宽，
     空着放一个灰圈只是噪声。 -->
{#if usage}
  <button
    onclick={() => (open = true)}
    aria-label="上下文用量"
    title={pctLabel == null
      ? `上下文 ${fmt(used)} token`
      : `上下文 ${pctLabel}（${fmt(used)} / ${fmt(ctxWindow ?? 0)}）`}
    class="grid size-9 shrink-0 place-items-center"
  >
    {@render ring(RING.size, RING.r, RING.sw, RING.fs)}
  </button>
{/if}

{#if open && usage}
  {@const total = usage.total}
  <!-- 高度贴合内容：这里就几行数字，别撑到半屏 -->
  <Sheet title="上下文用量" hug onclose={() => (open = false)}>
    <div class="flex flex-col gap-3 pb-2">
      <div class="flex items-center gap-4 rounded-2xl bg-surface px-4 py-4">
        {@render ring(RING_BIG.size, RING_BIG.r, RING_BIG.sw, RING_BIG.fs)}
        <div class="flex min-w-0 flex-col">
          <span class="text-[17px]">
            {fmt(used)}{#if ctxWindow}<span class="text-muted"> / {fmt(ctxWindow)}</span>{/if}
          </span>
          <span class="text-meta text-muted">
            {pctLabel == null
              ? '不知道这个模型的上下文上限（models.dev 里没有）'
              : `用掉 ${pctLabel} 的上下文`}
          </span>
        </div>
      </div>

      <!-- 本会话累计。字段对齐 DeepSeek Harness 的会话用量面板：
           缓存命中 / 未缓存输入 / 缓存读取 / 缓存写入（没写就不占行）/ 输出。
           数字仍然用 k / M 这种给人看的格式（DSH 那边是精确值）。 -->
      <div class="overflow-hidden rounded-2xl bg-surface">
        {#if total.cache_hit_ratio != null}
          <div class="flex items-baseline gap-3 border-t-2 border-ink px-3.5 py-3 first:border-t-0">
            <span class="text-[15px]">缓存命中</span>
            <span class="ml-auto text-[15px] text-muted">
              {fmtPct(total.cache_hit_ratio)}
            </span>
          </div>
        {/if}
        <div class="flex items-baseline gap-3 border-t-2 border-ink px-3.5 py-3 first:border-t-0">
          <span class="text-[15px]">未缓存输入</span>
          <span class="ml-auto text-[15px] text-muted">{fmt(total.uncached_input)} token</span>
        </div>
        <div class="flex items-baseline gap-3 border-t-2 border-ink px-3.5 py-3 first:border-t-0">
          <span class="text-[15px]">缓存读取</span>
          <span class="ml-auto text-[15px] text-muted">{fmt(total.cache_read)} token</span>
        </div>
        {#if total.cache_write > 0}
          <div class="flex items-baseline gap-3 border-t-2 border-ink px-3.5 py-3 first:border-t-0">
            <span class="text-[15px]">缓存写入</span>
            <span class="ml-auto text-[15px] text-muted">{fmt(total.cache_write)} token</span>
          </div>
        {/if}
        <div class="flex items-baseline gap-3 border-t-2 border-ink px-3.5 py-3 first:border-t-0">
          <span class="text-[15px]">输出</span>
          <span class="ml-auto text-[15px] text-muted">{fmt(total.output)} token</span>
        </div>
      </div>
    </div>
  </Sheet>
{/if}
