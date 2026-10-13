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
  const pct = $derived(ratio == null ? null : Math.round(ratio * 100));


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

  // 环的几何：直径 28（和同一行的 36px 圆钮配），轨道 2.6 粗
  const R = 11;
  const C = 2 * Math.PI * R;
  const dash = $derived(C * (1 - (ratio ?? 0)));
</script>

<!-- 环本身就是按钮。没有数据时**不占位** —— 输入框那一行本来就不宽，
     空着放一个灰圈只是噪声。 -->
{#if usage}
  <button
    onclick={() => (open = true)}
    aria-label="上下文用量"
    title={pct == null
      ? `上下文 ${fmt(used)} token`
      : `上下文 ${pct}%（${fmt(used)} / ${fmt(ctxWindow ?? 0)}）`}
    class="grid size-7 shrink-0 place-items-center"
  >
    <svg width="28" height="28" viewBox="0 0 28 28" fill="none" class="text-muted">
      <!-- 轨道 -->
      <circle cx="14" cy="14" r={R} stroke="currentColor" stroke-width="2.6" class="text-hairline" />
      <!-- 用量。没有上限时画一个空环（不给假信息） -->
      {#if ratio != null}
        <circle
          cx="14"
          cy="14"
          r={R}
          stroke="currentColor"
          stroke-width="2.6"
          stroke-linecap="round"
          stroke-dasharray={C}
          stroke-dashoffset={dash}
          transform="rotate(-90 14 14)"
        />
      {/if}
      {#if pct != null}
        <text
          x="14"
          y="14"
          text-anchor="middle"
          dominant-baseline="central"
          class="fill-current text-[9px]"
          style="font-variant-numeric: tabular-nums"
        >
          {pct}
        </text>
      {/if}
    </svg>
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
        <svg width="56" height="56" viewBox="0 0 28 28" fill="none" class="text-muted">
          <circle
            cx="14"
            cy="14"
            r={R}
            stroke="currentColor"
            stroke-width="2.6"
            class="text-control"
          />
          {#if ratio != null}
            <circle
              cx="14"
              cy="14"
              r={R}
              stroke="currentColor"
              stroke-width="2.6"
              stroke-linecap="round"
              stroke-dasharray={C}
              stroke-dashoffset={dash}
              transform="rotate(-90 14 14)"
            />
          {/if}
          {#if pct != null}
            <text
              x="14"
              y="14"
              text-anchor="middle"
              dominant-baseline="central"
              class="fill-current text-[9px]"
              style="font-variant-numeric: tabular-nums"
            >
              {pct}
            </text>
          {/if}
        </svg>
        <div class="flex min-w-0 flex-col">
          <span class="text-[17px]">
            {fmt(used)}{#if ctxWindow}<span class="text-muted"> / {fmt(ctxWindow)}</span>{/if}
          </span>
          <span class="text-meta text-muted">
            {pct == null ? '不知道这个模型的上下文上限（models.dev 里没有）' : `用掉 ${pct}% 的上下文`}
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

      <p class="px-1 text-meta leading-relaxed text-muted">
        上下文 = 最近一次请求发出去的 token（整段历史 + 系统提示词 + 工具）。缓存命中率是
        命中的前缀占比 —— 命中部分便宜得多，它掉了说明前缀变了。
      </p>
    </div>
  </Sheet>
{/if}
