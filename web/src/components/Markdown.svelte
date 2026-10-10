<script lang="ts">
  /**
   * 渲染 markdown 块。走 Svelte 组件而不是 {@html}：
   * 没有 XSS 面，样式也全在 Tailwind 里。
   */
  import { untrack } from 'svelte';
  import { parse, type Span } from '../lib/markdown';

  let { text }: { text: string } = $props();

  // 流式时每个 delta 都会改 text，一帧里可能来好几个。整段重新解析 +
  // 重新渲染一帧做一次就够了：变化先攒着，下一帧取最新值。
  // 历史消息首帧就是完整的，不经过这里。
  let shown = $state(untrack(() => text));
  let raf = 0;
  $effect(() => {
    void text;
    raf ||= requestAnimationFrame(() => {
      raf = 0;
      shown = text;
    });
  });
  $effect(() => () => cancelAnimationFrame(raf));

  const blocks = $derived(parse(shown));
</script>

{#snippet spans(list: Span[])}
  {#each list as s}
    {#if s.t === 'text'}{s.v}
    {:else if s.t === 'code'}<code class="rounded bg-control px-1 py-px font-mono text-[0.88em]">{s.v}</code>
    {:else if s.t === 'strong'}<strong class="font-semibold">{s.v}</strong>
    {:else if s.t === 'em'}<em class="italic">{s.v}</em>
    {:else}<a href={s.href} class="underline decoration-faint underline-offset-2">{s.v}</a>
    {/if}
  {/each}
{/snippet}

<div class="flex flex-col gap-2.5">
  {#each blocks as b}
    {#if b.t === 'p'}
      <p class="whitespace-pre-wrap break-words">{@render spans(b.spans)}</p>
    {:else if b.t === 'h'}
      <p class="font-semibold {b.level <= 2 ? 'text-lg' : 'text-base'}">{@render spans(b.spans)}</p>
    {:else if b.t === 'code'}
      <div class="overflow-hidden rounded-xl bg-hairline-soft">
        {#if b.lang}
          <p class="border-b border-hairline px-3 py-1.5 text-meta text-faint">{b.lang}</p>
        {/if}
        <pre class="overflow-x-auto px-3 py-2.5 font-mono text-[13px] leading-5">{b.v}</pre>
      </div>
    {:else if b.t === 'ul'}
      <ul class="flex flex-col gap-1.5 pl-5">
        {#each b.items as it}
          <li class="list-disc whitespace-pre-wrap break-words">{@render spans(it)}</li>
        {/each}
      </ul>
    {:else if b.t === 'ol'}
      <ol class="flex flex-col gap-1.5 pl-5">
        {#each b.items as it}
          <li class="list-decimal whitespace-pre-wrap break-words">{@render spans(it)}</li>
        {/each}
      </ol>
    {:else if b.t === 'quote'}
      <blockquote class="border-l-2 border-hairline pl-3 text-muted">
        {@render spans(b.spans)}
      </blockquote>
    {/if}
  {/each}
</div>
