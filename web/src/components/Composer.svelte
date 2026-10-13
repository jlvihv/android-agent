<script lang="ts">
  import Icon from './Icon.svelte';
  import AttachSheet from './AttachSheet.svelte';
  import ContextMeter from './ContextMeter.svelte';
  import { store } from '../lib/store.svelte.ts';
  import { effortLabel } from '../lib/effort';
  import type { Upload, SessionUsage } from '../lib/sse';

  let {
    value = $bindable(''),
    busy = false,
    usage = null,
    ctxWindow = null,
    onsend,
    onstop,
    onmodel,
  }: {
    value: string;
    busy: boolean;
    /** 本会话的 token 用量（输入框里的上下文圆环）。`null` = 还没跑过 */
    usage?: SessionUsage | null;
    /** 模型上下文上限（token）；不知道就是 null */
    ctxWindow?: number | null;
    /** 发送时把这次要带的附件一起交出去（本组件随即清空胶囊） */
    onsend: (attachments: Upload[]) => void;
    onstop: () => void;
    onmodel: () => void;
  } = $props();

  /**
   * 输入卡片里的一个胶囊。分三态，是为了让「选完之后到底成没成」一眼可见：
   * 上传中给转圈，失败给红条 + 原因，成功才显示缩略图/文件名。
   */
  type Chip =
    | { key: string; state: 'uploading'; name: string }
    | { key: string; state: 'error'; name: string; error: string }
    | ({ key: string; state: 'ready' } & Upload);

  let ta: HTMLTextAreaElement | null = $state(null);
  let attach = $state(false);
  let chips = $state<Chip[]>([]);

  /** 已上传成功、等着随这条消息发出去的附件 */
  const ready = $derived(
    chips.filter((c) => c.state === 'ready') as Extract<Chip, { state: 'ready' }>[],
  );
  const uploading = $derived(chips.some((c) => c.state === 'uploading'));
  const failed = $derived(chips.filter((c) => c.state === 'error'));

  // 三个隐藏的文件输入。相机的 `capture="environment"` 让 WebView 直接
  // 拉起后置摄像头；照片和文件都交给系统选择器（壳里的 onShowFileChooser）。
  let cameraInput: HTMLInputElement | null = $state(null);
  let photoInput: HTMLInputElement | null = $state(null);
  let fileInput: HTMLInputElement | null = $state(null);

  // 「可以发」不再看 busy：模型还在说的时候发的下一条进队列，等它收尾再发
  // （见 App.svelte 的 send / flushQueue）。所以 streaming 时输入区是**活的**，
  // 只是右边多一个停止键。
  const canSend = $derived(
    (value.trim().length > 0 || ready.length > 0) && !uploading,
  );

  // 按内容长高，最多 5 行（再高就在框里滚）：5 × 行高 1.5rem + 上下内边距
  // 0.25×2 = 8rem。这个数是算出来的，不是量的。
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
      if (canSend) send();
    }
  }

  function send() {
    if (!canSend) return;
    const atts = ready;
    chips = [];
    attach = false;
    onsend(atts);
  }

  /** 在底部面板里选了入口：先收起面板，再拉起对应的文件选择器 */
  function onAttachPick(kind: 'camera' | 'photo' | 'file') {
    attach = false;
    const input = kind === 'camera' ? cameraInput : kind === 'photo' ? photoInput : fileInput;
    input?.click();
  }

  /**
   * 选好文件（可能多选）。**先占位、再上传**：占位那一颗转圈就是「我收到了」
   * 的即时反馈 —— 没有它的话，大图上传那几秒界面上什么都没发生，用户根本
   * 不知道选中的文件到底进没进来。
   */
  async function onPick(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    const files = Array.from(input.files ?? []);
    input.value = '';
    if (files.length === 0) return;

    for (const f of files) {
      const key = `${f.name}-${f.size}-${Date.now()}-${Math.random().toString(36).slice(2)}`;
      chips = [...chips, { key, state: 'uploading', name: f.name }];
      try {
        const up = await upload(f);
        chips = chips.map((c) => (c.key === key ? { key, state: 'ready', ...up } : c));
      } catch (err) {
        chips = chips.map((c) =>
          c.key === key ? { key, state: 'error', name: f.name, error: String(err) } : c,
        );
      }
    }
  }

  async function upload(f: File): Promise<Upload> {
    const r = await fetch('/api/attachment', {
      method: 'POST',
      headers: {
        'content-type': f.type || 'application/octet-stream',
        // 文件名可能带中文：HTTP 头只能是 ISO-8859-1，先编码
        'x-filename': encodeURIComponent(f.name),
      },
      body: f,
    });
    if (!r.ok) throw new Error((await r.text()) || `上传失败（${r.status}）`);
    return (await r.json()) as Upload;
  }

  function remove(key: string) {
    chips = chips.filter((c) => c.key !== key);
  }
</script>

<!-- 输入卡片：文字在上、控件在下，整体是一块浮起的圆角面。

     间距不是拍脑袋的，是对着 Claude 安卓端量出来的（Pixel 9 截图，412dp 宽）：
     卡片离屏幕左右 16、卡片内边距 8、圆钮直径 36、圆钮之间 8、文字区自己再
     内缩 4（于是文字落在 12）、文字区到控件行 16。全都能落在 Tailwind 刻度上：
     mx-4 / p-2 / size-9 / gap-2 / px-1 py-1 / gap-4。 -->
<div
  class="mx-4 mb-[var(--composer-gap)] flex flex-col gap-4
         rounded-card border border-control bg-surface p-2"
>
  {#if chips.length > 0}
    <!-- 附件胶囊：上传中转圈 / 失败红条 / 成功给缩略图或文件名 -->
    <div class="flex flex-wrap gap-2 px-1">
      {#each chips as c (c.key)}
        <div class="relative">
          {#if c.state === 'ready'}
            {#if c.kind === 'image'}
              <img
                src={c.url}
                alt={c.name}
                class="size-14 rounded-xl border border-hairline object-cover"
              />
            {:else}
              <span
                class="flex h-14 max-w-44 items-center gap-1.5 rounded-xl border border-hairline
                       bg-hairline-soft px-2.5 text-meta text-fg"
              >
                <Icon name="file" size={16} class="shrink-0 text-muted" />
                <span class="truncate">{c.name}</span>
              </span>
            {/if}
          {:else}
            <span
              class="flex h-14 max-w-44 items-center gap-2 rounded-xl border px-3 text-meta
                     {c.state === 'error'
                ? 'border-bad/50 bg-hairline-soft text-bad'
                : 'border-hairline bg-hairline-soft text-muted'}"
            >
              {#if c.state === 'uploading'}
                <span
                  class="size-4 shrink-0 animate-spin rounded-full border-2 border-control
                         border-t-fg"
                ></span>
                <span class="truncate">上传中…</span>
              {:else}
                <Icon name="close" size={14} class="shrink-0" />
                <span class="truncate">上传失败</span>
              {/if}
            </span>
          {/if}
          <button
            onclick={() => remove(c.key)}
            aria-label="移除附件"
            class="absolute -top-1.5 -right-1.5 grid size-5 place-items-center rounded-full
                   bg-ink text-fg ring-1 ring-hairline"
          >
            <Icon name="close" size={11} />
          </button>
        </div>
      {/each}
    </div>
  {/if}

  <textarea
    bind:this={ta}
    bind:value
    rows="1"
    placeholder="与 agent 对话…"
    onkeydown={onkeydown}
    spellcheck="false"
    class="max-h-[8rem] resize-none overflow-y-auto bg-transparent px-1 py-1 text-base leading-6
           text-fg placeholder:text-muted outline-none"
  ></textarea>

  {#if failed.length > 0}
    <!-- 失败原因放一行完整显示：缩略胶囊里塞不下，而「为什么没加进去」正是此刻最该看到的 -->
    <p class="px-1 text-xs text-bad">
      {failed.map((f) => `${f.name}：${f.error}`).join('；')}
    </p>
  {/if}

  <!-- 控件行 -->
  <div class="flex items-center gap-2">
    <button
      onclick={() => (attach = true)}
      aria-label="添加文件"
      class="grid size-9 shrink-0 place-items-center rounded-full bg-control text-fg"
      class:opacity-60={uploading}
      disabled={uploading}
    >
      <Icon name="plus" size={19} />
    </button>

    <!-- 模型药丸：模型名 + **当前思考强度**。两个都要看得见 ——
         强度是会直接影响“要等多久”的开关，藏在面板里没道理。
         强度用 muted：它是副信息，但读得出来。 -->
    <button
      onclick={onmodel}
      title="模型 / 思考强度"
      class="flex h-9 max-w-[52%] shrink-0 items-center rounded-full bg-control px-4 text-sm
             text-fg"
    >
      <span class="min-w-0 truncate">{store.llm?.model ?? '未配置模型'}</span>
      {#if store.llm}
        <span class="ml-1.5 shrink-0 text-muted">· {effortLabel(store.llm.reasoningEffort)}</span>
      {/if}
    </button>

    <!-- 上下文用量：一个环，点开看明细（见 ContextMeter） -->
    <ContextMeter {usage} {ctxWindow} />

    <!-- 右边一组：还在跑 → 停止键（左边）；有字/附件或空闲 → 发送键（可以两个同时在）。
         空闲时发送键也**一定在**（暗着）—— 不能让它一会出现一会消失。 -->
    <div class="ml-auto flex shrink-0 items-center gap-2">
      {#if busy}
        <button
          onclick={onstop}
          aria-label="停止"
          class="grid size-9 shrink-0 place-items-center rounded-full bg-control text-fg"
        >
          <Icon name="stop" size={18} />
        </button>
      {/if}
      {#if !busy || canSend}
        <!-- 发送：空态是暗的，有字或附件才变白底黑箭头。
             还在跑时发出去的是「排队」，title 说清楚。 -->
        <button
          onclick={send}
          disabled={!canSend}
          aria-label={busy ? '排队发送（agent 说完就发）' : '发送'}
          title={busy ? '排队发送：agent 说完这条就发出去' : '发送'}
          class="grid size-9 shrink-0 place-items-center rounded-full transition-colors
                 {canSend ? 'bg-fg text-ink' : 'bg-control text-faint'}"
        >
          <Icon name="send" size={18} />
        </button>
      {/if}
    </div>
  </div>

  <input
    bind:this={cameraInput}
    type="file"
    accept="image/*"
    capture="environment"
    onchange={onPick}
    class="hidden"
  />
  <input bind:this={photoInput} type="file" accept="image/*" multiple onchange={onPick} class="hidden" />
  <input
    bind:this={fileInput}
    type="file"
    accept="application/pdf,.pdf"
    multiple
    onchange={onPick}
    class="hidden"
  />
</div>

{#if attach}
  <AttachSheet onclose={() => (attach = false)} onpick={onAttachPick} />
{/if}
