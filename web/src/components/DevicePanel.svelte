<script lang="ts">
  /** 界面面板：当前屏幕的 UI 树。从抽屉打开。 */
  import { store } from '../lib/store.svelte.ts';
  import type { UiNode } from '../lib/types';
  import Icon from './Icon.svelte';

  const rows = $derived.by(() => {
    const snap = store.snapshot;
    if (!snap) return [] as { node: UiNode; depth: number }[];
    const depth = new Map<number, number>();
    return snap.nodes.map((node) => {
      const d = node.p >= 0 ? (depth.get(node.p) ?? 0) + 1 : 0;
      depth.set(node.i, d);
      return { node, depth: d };
    });
  });

  const label = (n: UiNode) => n.t || n.id || n.c;
  const sub = (n: UiNode) => {
    const [l, t, r, b] = n.b;
    return [n.id, `${r - l}×${b - t}`, n.s].filter(Boolean).join(' · ');
  };
  const caps = (n: UiNode) =>
    [
      n.a.clickable && 'click',
      n.a.editable && 'edit',
      n.a.scrollable && 'scroll',
      n.a.checkable && 'check',
      !n.a.enabled && 'disabled',
    ].filter(Boolean) as string[];
</script>

<div class="flex h-full flex-col">
  <div class="shrink-0 border-b border-hairline px-4 py-2.5">
    {#if store.snapshot}
      <div class="flex items-baseline gap-2 text-xs">
        <span class="truncate font-mono text-fg">{store.snapshot.package}</span>
        <span class="ml-auto shrink-0 font-mono text-faint">{store.snapshot.nodes.length} 节点</span>
      </div>
      <div class="truncate font-mono text-meta text-faint">
        {store.snapshot.activity} · {store.snapshot.screen.w}×{store.snapshot.screen.h}
      </div>
    {:else}
      <div class="text-xs text-faint">还没有快照</div>
    {/if}
    <div class="mt-2.5 flex gap-2">
      <button
        onclick={() => store.refresh()}
        class="inline-flex items-center gap-1 rounded-full bg-hairline px-3 py-1.5 text-xs"
      >
        <Icon name="refresh" size={14} /> 刷新
      </button>
      <button
        onclick={() => store.command({ type: 'key', name: 'back' })}
        class="inline-flex items-center gap-1 rounded-full bg-hairline px-3 py-1.5 text-xs"
      >
        <Icon name="back" size={14} /> 返回键
      </button>
    </div>
  </div>

  <div class="min-h-0 flex-1 overflow-y-auto pb-3">
    {#if rows.length === 0}
      <p class="py-8 text-center text-xs text-faint">点「刷新」读一次 UI 树</p>
    {:else}
      {#each rows as { node, depth } (node.i)}
        <div
          class="flex items-center gap-2 border-b border-hairline-soft py-1.5 pr-4"
          style="padding-left: calc(1rem + {depth} * 0.625rem)"
        >
          <div class="min-w-0 flex-1">
            <div class="flex items-center gap-1.5">
              <span class="truncate text-[13px] text-fg">{label(node)}</span>
              <span class="shrink-0 font-mono text-meta text-faint">#{node.i}</span>
            </div>
            <div class="truncate font-mono text-meta text-faint">{node.c} · {sub(node)}</div>
            {#if caps(node).length}
              <div class="mt-1 flex flex-wrap gap-1">
                {#each caps(node) as c (c)}
                  <span
                    class="rounded font-mono text-[9px] px-1 py-px {c === 'disabled'
                      ? 'bg-bad/15 text-bad'
                      : 'bg-ok/15 text-ok'}">{c}</span
                  >
                {/each}
              </div>
            {/if}
          </div>
          {#if node.a.clickable}
            <button
              onclick={() => store.command({ type: 'tap_node', i: node.i })}
              class="shrink-0 rounded-full bg-hairline px-3 py-1 text-xs"
            >
              点
            </button>
          {/if}
        </div>
      {/each}
    {/if}
  </div>
</div>
