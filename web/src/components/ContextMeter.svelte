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
   *   上下文   = 最近一次请求的 `prompt_tokens`（整段历史 + 系统提示词 + 工具）
   *   输入/输出 = 最近一次调用的 `prompt_tokens` / `completion_tokens`
   *   缓存命中 = 命中的缓存 token ÷ 输入（同一套两套写的字段，见 `cachedTokens`）
   */
  import { cachedTokens, cacheHitRatio, reasoningTokens, type SessionUsage } from '../lib/sse';
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

  const used = $derived(usage?.last.prompt_tokens ?? 0);
  /** 0..1；没有窗口就是 null */
  const ratio = $derived(ctxWindow ? Math.min(1, used / ctxWindow) : null);
  /**
   * 环里那两个字。四舍五入到整数，但**明明有量却不到 0.5% 时不写 0** ——
   * 1M 窗口的头几轮就是这样（实测第一句 prompt 才 1468），写个 0 看着像坏了。
   */
  const ringText = $derived(
    ratio == null ? null : ratio > 0 && ratio * 100 < 0.5 ? '<1' : String(Math.round(ratio * 100)),
  );
  /** 面板和 tooltip 用的百分比：不到 1% 时给一位小数（0.1% 比 0% 诚实） */
  const pctLabel = $derived(
    ratio == null
      ? null
      : ratio >= 0.01
        ? `${Math.round(ratio * 100)}%`
        : `${(ratio * 100).toFixed(1)}%`,
  );


  // —— 数字排版 ——
  //
  // token 数只保留三位有效：`1.2M` / `36.2k` / `940`。面板是给人瞟一眼的，
  // 精确到个位只是噪声（真实值在 /api/session 里，随时能看）。
  function fmt(n: number): string {
    if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n >= 10_000_000 ? 0 : 1)}M`;
    if (n >= 1_000) return `${(n / 1_000).toFixed(n >= 10_000 ? 0 : 1)}k`;
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
    {@render ring(36, 15.4, 3, 12)}
  </button>
{/if}

{#if open && usage}
  {@const u = usage.last}
  {@const hit = cachedTokens(u)}
  {@const hr = cacheHitRatio(u)}
  {@const reason = reasoningTokens(u)}
  <!-- 高度贴合内容：这里就几行数字，别撑到半屏 -->
  <Sheet title="上下文用量" minClass="min-h-0" onclose={() => (open = false)}>
    <div class="flex flex-col gap-3 pb-2">
      <div class="flex items-center gap-4 rounded-2xl bg-surface px-4 py-4">
        {@render ring(56, 22, 5.2, 18)}
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

      <div class="overflow-hidden rounded-2xl bg-surface">
        <div class="flex items-baseline gap-3 border-b-2 border-ink px-3.5 py-3">
          <span class="text-[15px]">最近一次调用</span>
          <span class="ml-auto text-[15px] text-muted">
            输入 {fmt(u.prompt_tokens)} · 输出 {fmt(u.completion_tokens)}
          </span>
        </div>
        {#if reason != null}
          <div class="flex items-baseline gap-3 border-b-2 border-ink px-3.5 py-3">
            <span class="text-[15px]">其中思考</span>
            <span class="ml-auto text-[15px] text-muted">{fmt(reason)} token</span>
          </div>
        {/if}
        <div class="flex items-baseline gap-3 px-3.5 py-3">
          <span class="text-[15px]">缓存命中</span>
          <span class="ml-auto text-[15px] text-muted">
            {#if hr == null || hit == null}
              provider 没报（这个端点不支持）
            {:else}
              {fmt(hit)} / {fmt(u.prompt_tokens)} · {fmtPct(hr)}
            {/if}
          </span>
        </div>
      </div>

      <div class="overflow-hidden rounded-2xl bg-surface">
        <div class="flex items-baseline gap-3 px-3.5 py-3">
          <span class="text-[15px]">本会话累计</span>
          <span class="ml-auto text-[15px] text-muted">
            输入 {fmt(usage.total.input)} · 输出 {fmt(usage.total.output)}
          </span>
        </div>
        <div class="flex items-baseline gap-3 border-t-2 border-ink px-3.5 py-3">
          <span class="text-[15px]">调用次数</span>
          <span class="ml-auto text-[15px] text-muted">
            {usage.total.calls} 次{#if usage.total.input > 0 && usage.total.cached > 0}
              · 命中 {fmtPct(usage.total.cached / usage.total.input)}{/if}
          </span>
        </div>
      </div>
    </div>
  </Sheet>
{/if}
