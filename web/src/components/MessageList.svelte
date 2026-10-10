<script lang="ts">
  /**
   * 消息列表。对齐 Claude 的层级：
   *   用户 —— 右侧浅色气泡
   *   助手 —— 左侧无气泡，纯文本全宽
   *   工具调用 —— 细边卡片，缩进，不抢视觉
   */
  import type { Message, ToolPart } from '../lib/sse';
  import Icon from './Icon.svelte';

  let {
    messages,
    ondecide,
  }: {
    messages: Message[];
    ondecide: (msg: Message, part: ToolPart, approved: boolean) => void;
  } = $props();

  let scroller: HTMLDivElement | null = $state(null);
  let showReasoning = $state(false);
  let open = $state<string | null>(null);

  /**
   * 把工具调用压成一句人话。Claude 的中间步骤就是这样 ——
   * 一行截断描述，而不是把 JSON 摊在正文里。
   */
  function summarize(t: ToolPart): string {
    const a = (t.input ?? {}) as Record<string, unknown>;
    const s = (v: unknown) => (typeof v === 'string' ? v : '');
    const firstLine = (v: string) => v.split('\n')[0].slice(0, 70);

    switch (t.toolName) {
      case 'bash':
        return firstLine(s(a.command)) || '执行命令';
      case 'edit':
        return `编辑 ${s(a.path) || '文件'}`;
      case 'device_snapshot':
        return '读取当前界面';
      case 'device_tap':
        return `点击节点 #${a.i ?? '?'}`;
      case 'device_type':
        return `向 #${a.i ?? '?'} 输入文字`;
      case 'device_key':
        return `按 ${s(a.name) || '键'}`;
      case 'device_launch':
        return `启动 ${s(a.package) || '应用'}`;
      default:
        return t.toolName || '工具调用';
    }
  }

  // 新内容贴底
  $effect(() => {
    void messages.length;
    void messages.at(-1)?.parts.length;
    if (scroller) scroller.scrollTop = scroller.scrollHeight;
  });

  const json = (v: unknown) => (v === undefined ? '' : JSON.stringify(v, null, 2));
</script>

<div bind:this={scroller} class="flex min-h-0 flex-1 flex-col gap-6 overflow-y-auto px-4 pt-2 pb-4">
  {#each messages as msg (msg.id)}
    {#if msg.role === 'user'}
      <div class="flex justify-end">
        <p class="max-w-[84%] rounded-card bg-bubble px-3.5 py-2.5 text-base leading-6 whitespace-pre-wrap break-words">
          {msg.parts.map((p) => (p.type === 'text' ? p.text : '')).join('')}
        </p>
      </div>
    {:else}
      <div class="flex flex-col gap-2.5 text-base leading-relaxed">
        {#each msg.parts as part, i (i)}
          {#if part.type === 'text'}
            <p class="whitespace-pre-wrap break-words">{part.text}</p>
          {:else if part.type === 'reasoning'}
            <div class="border-l-2 border-hairline pl-2.5">
              <button
                onclick={() => (showReasoning = !showReasoning)}
                class="text-xs text-muted"
              >
                思考 {showReasoning ? '▾' : '▸'}
              </button>
              {#if showReasoning}
                <p class="mt-1 text-[13px] leading-5 whitespace-pre-wrap text-muted">{part.text}</p>
              {/if}
            </div>
          {:else if part.type === 'tool'}
            {@const tool = part}
            <!-- 折叠条：一行摘要 + 展开。对齐 Claude 的中间步骤渲染，
                 不是一直摊开的卡片（那样会把正文挤没）。 -->
            <button
              onclick={() => (open = open === tool.toolCallId ? null : tool.toolCallId)}
              class="flex w-full items-center gap-2 py-0.5 text-left"
            >
              <Icon name="clock" size={16} class="shrink-0 text-faint" />
              <span class="truncate text-sm text-muted">{summarize(tool)}</span>
              {#if tool.state === 'awaiting-approval'}
                <span class="shrink-0 text-meta text-warn">待批准</span>
              {:else if tool.state === 'denied'}
                <span class="shrink-0 text-meta text-bad">已拒绝</span>
              {/if}
              <Icon
                name="chevron"
                size={15}
                class="ml-auto shrink-0 text-faint transition-transform
                       {open === tool.toolCallId ? 'rotate-90' : ''}"
              />
            </button>

            {#if open === tool.toolCallId}
              <div class="ml-6 flex flex-col gap-2 rounded-xl border border-hairline bg-[#1b1b1a] px-2.5 py-2">
                {#if tool.input !== undefined}
                  <pre class="max-h-56 overflow-auto font-mono text-meta whitespace-pre-wrap break-all text-muted">{json(
                      tool.input,
                    )}</pre>
                {:else if tool.inputText}
                  <pre class="max-h-56 overflow-auto font-mono text-meta whitespace-pre-wrap break-all text-faint">{tool.inputText}</pre>
                {/if}
                {#if tool.approvalReason}
                  <p class="text-xs text-warn">{tool.approvalReason}</p>
                {/if}
                {#if tool.state === 'awaiting-approval'}
                  <div class="flex gap-2">
                    <button
                      onclick={() => ondecide(msg, tool, true)}
                      class="rounded-full bg-fg px-3.5 py-1 text-xs text-ink"
                    >
                      批准
                    </button>
                    <button
                      onclick={() => ondecide(msg, tool, false)}
                      class="rounded-full bg-control px-3.5 py-1 text-xs text-fg"
                    >
                      拒绝
                    </button>
                  </div>
                {/if}
                {#if tool.output !== undefined}
                  <pre class="max-h-56 overflow-auto border-t border-hairline pt-2 font-mono text-meta whitespace-pre-wrap break-all text-muted">{json(
                      tool.output,
                    )}</pre>
                {/if}
              </div>
            {/if}
          {:else if part.type === 'data'}
            <!-- 用量不显示：Claude 没有，看着啰嗦 -->
          {:else if part.type === 'error'}
            <p class="text-[13px] text-bad">错误：{part.text}</p>
          {/if}
        {/each}

        {#if !msg.done}
          <span class="inline-block h-4 w-1.5 animate-pulse bg-muted"></span>
        {/if}
      </div>
    {/if}
  {/each}
</div>
