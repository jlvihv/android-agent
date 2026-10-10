<script lang="ts">
  /** 事件面板：指令回执 + 日志。从抽屉打开。 */
  import { store } from '../lib/store.svelte.ts';

  const lv = {
    error: 'text-bad',
    warn: 'text-warn',
    info: 'text-muted',
    debug: 'text-faint',
  } as Record<string, string>;

  const hhmmss = (ts: number) =>
    new Date(ts).toTimeString().slice(0, 8);
</script>

<div class="flex h-full flex-col">
  <div class="flex shrink-0 items-center border-b border-hairline px-4 py-2.5 text-meta text-faint">
    {store.logs.length} 条日志 · {store.acks.length} 条回执
    <button onclick={() => store.clearLogs()} class="ml-auto rounded-full bg-hairline px-3 py-1 text-xs text-fg">
      清空
    </button>
  </div>

  <div class="min-h-0 flex-1 overflow-y-auto">
    {#if store.acks.length}
      <h2 class="px-4 pt-3 pb-1 text-[10px] uppercase tracking-widest text-faint">指令回执</h2>
      {#each store.acks as a (a.id)}
        <div class="selectable border-b border-hairline-soft px-4 py-1.5 font-mono text-meta">
          <span class={a.ok ? 'text-ok' : 'text-bad'}>{a.ok ? 'ok' : 'err'}</span>
          <span class="ml-2 text-faint">{hhmmss(a.ts)}</span>
          <div class="text-muted">{a.detail}</div>
        </div>
      {/each}
    {/if}

    <h2 class="px-4 pt-3 pb-1 text-[10px] uppercase tracking-widest text-faint">日志</h2>
    {#if store.logs.length === 0}
      <p class="py-5 text-center text-meta text-faint">暂无</p>
    {:else}
      {#each store.logs as l, i (i)}
        <div class="selectable flex gap-2 border-b border-hairline-soft px-4 py-1 font-mono text-meta">
          <span class="shrink-0 text-faint">{hhmmss(l.ts)}</span>
          <span class="shrink-0 {lv[l.level] ?? 'text-muted'}">{l.level}</span>
          <span class="min-w-0 break-all text-muted">{l.msg}</span>
        </div>
      {/each}
    {/if}
  </div>
</div>
