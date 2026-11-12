<script lang="ts">
  /**
   * 设置页「用量」一栏：按模型（全部历史）、按天（最近 14 天）的 token 和缓存命中。
   *
   * 只有 token，**不估钱**：包月套餐（OpenCode Go 这类）按目录价算出来的数字是错的，
   * 不如不给（见 core 的 `usage_summary`）。输入含缓存读取。
   */
  import { api } from "../lib/api";
  import { fmtCount } from "../lib/num";

  type Row = {
    key: string;
    calls: number;
    input: number;
    cacheRead: number;
    output: number;
    hitRatio: number | null;
  };

  let byModel = $state<Row[]>([]);
  let byDay = $state<Row[]>([]);
  let loaded = $state(false);
  let showDays = $state(false);

  $effect(() => {
    void (async () => {
      try {
        const d = await api<{ byModel: Row[]; byDay: Row[] }>("/api/usage");
        byModel = d.byModel;
        byDay = d.byDay;
      } catch {
        // 拉不到就不显示这一栏（`loaded` 仍然置真）：用量是附加信息，
        // 不值得为它在设置页里挂一条错误。以前这里没有 `catch`，fetch 一失败
        // 就是一个 unhandled rejection。
      } finally {
        loaded = true;
      }
    })();
  });

  /** 和上下文圆环同一套（`lib/num.ts`）：三位有效数字，k / M */
  const fmt = fmtCount;
  const pct = (r: number | null) =>
    r == null ? "-" : `${Math.round(r * 100)}%`;
  /** 一行的第二行字：次数 · 输入 · 输出 · 命中 */
  const line = (r: Row) =>
    `${r.calls} 次 · 输入 ${fmt(r.input)} · 输出 ${fmt(r.output)} · 缓存命中 ${pct(r.hitRatio)}`;
</script>

{#if loaded && byModel.length}
  <h2 class="px-1 pt-6 pb-2 text-meta text-muted">用量</h2>
  <div class="overflow-hidden rounded-2xl bg-surface">
    {#each byModel as r (r.key)}
      <div class="border-b-2 border-ink px-3.5 py-3 last:border-b-0">
        <span class="block truncate text-row">{r.key}</span>
        <span class="block text-meta text-muted">{line(r)}</span>
      </div>
    {/each}
  </div>
  {#if byDay.length}
    <button
      onclick={() => (showDays = !showDays)}
      class="px-1 pt-2 text-meta text-accent"
    >
      {showDays ? "收起" : "最近 14 天每天"}
    </button>
    {#if showDays}
      <div class="mt-2 overflow-hidden rounded-2xl bg-surface">
        {#each byDay as r (r.key)}
          <div class="border-b-2 border-ink px-3.5 py-2.5 last:border-b-0">
            <span class="block text-ui">{r.key}</span>
            <span class="block text-meta text-muted">{line(r)}</span>
          </div>
        {/each}
      </div>
    {/if}
  {/if}
{/if}
