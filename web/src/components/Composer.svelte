<script lang="ts">
  import Icon from './Icon.svelte';
  import { store } from '../lib/store.svelte.ts';

  let {
    value = $bindable(''),
    busy = false,
    onsend,
    onstop,
    onmenu,
    onmodel,
  }: {
    value: string;
    busy: boolean;
    onsend: () => void;
    onstop: () => void;
    onmenu: () => void;
    onmodel: () => void;
  } = $props();

  let ta: HTMLTextAreaElement | null = $state(null);

  const canSend = $derived(value.trim().length > 0 && !busy);

  // 按内容长高，最多 5 行，再多就在框里滚。
  // 原来按换行符数算 rows，一行长文字自动折行时框不长，只能在一行里滚。
  $effect(() => {
    void value;
    if (!ta) return;
    ta.style.height = 'auto';
    ta.style.height = `${ta.scrollHeight}px`;
  });

  export const focus = () => ta?.focus();

  function onkeydown(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      if (canSend) onsend();
    }
  }
</script>

<!-- 输入卡片：文字在上、控件在下，整体是一块浮起的圆角面 -->
<div
  class="mx-5 mb-[var(--composer-gap)] flex flex-col gap-3
         rounded-card border border-control bg-surface p-3"
>
  <textarea
    bind:this={ta}
    bind:value
    rows="1"
    placeholder="与 agent 对话…"
    onkeydown={onkeydown}
    spellcheck="false"
    class="resize-none bg-transparent px-1 pt-1 text-base leading-6 text-fg
           placeholder:text-muted outline-none"
    style="max-height: 8.5rem; overflow-y: auto"
  ></textarea>

  <div class="flex items-center gap-2">
    <button
      onclick={onmenu}
      aria-label="菜单"
      class="grid size-9 shrink-0 place-items-center rounded-full bg-control text-fg"
    >
      <Icon name="plus" size={19} />
    </button>

    <button
      onclick={onmodel}
      class="h-9 max-w-[60%] shrink-0 truncate rounded-full bg-control px-4 text-sm text-fg"
    >
      {store.llm?.model ?? '未配置模型'}
    </button>

    <div class="flex-1"></div>

    {#if busy}
      <button
        onclick={onstop}
        aria-label="停止"
        class="grid size-9 shrink-0 place-items-center rounded-full bg-control text-fg"
      >
        <Icon name="stop" size={18} />
      </button>
    {:else}
      <!-- 发送：空态是暗的，有字才变白底黑箭头 -->
      <button
        onclick={onsend}
        disabled={!canSend}
        aria-label="发送"
        class="grid size-9 shrink-0 place-items-center rounded-full transition-colors
               {canSend ? 'bg-fg text-ink' : 'bg-control text-faint'}"
      >
        <Icon name="send" size={18} />
      </button>
    {/if}
  </div>
</div>
