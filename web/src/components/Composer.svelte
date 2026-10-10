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
  const rows = $derived(Math.min(5, value.split('\n').length));

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
  class="mx-4 mb-[calc(1rem+env(safe-area-inset-bottom))] flex flex-col gap-3
         rounded-card border border-hairline bg-surface p-3"
>
  <textarea
    bind:this={ta}
    bind:value
    {rows}
    placeholder="与 agent 对话…"
    onkeydown={onkeydown}
    spellcheck="false"
    class="resize-none bg-transparent px-1 pt-1 text-base leading-6 text-fg
           placeholder:text-muted outline-none"
    style="max-height: 8.5rem"
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
