<script lang="ts">
  /**
   * 消息列表。对齐 Claude 的层级：
   *   用户 —— 右侧浅色气泡
   *   助手 —— 左侧无气泡，markdown 全宽
   *   思考 / 工具调用 —— 一行摘要，点开是底部面板
   *
   * 为什么用面板而不是就地展开：中间步骤（尤其 bash 输出）很长，就地展开
   * 会把正文挤没；而且正文是流式的，展开会让内容往下跳。面板是覆盖层，
   * 不参与正文布局。
   *
   * 每个思考、每次工具调用**各开各的面板**，面板里只有它自己。
   */
  import { tick } from 'svelte';
  import type { Message, ToolPart } from '../lib/sse';
  import Icon from './Icon.svelte';
  import Markdown from './Markdown.svelte';
  import Sheet from './Sheet.svelte';
  import ThoughtLine from './ThoughtLine.svelte';

  let { messages }: { messages: Message[] } = $props();

  let scroller: HTMLDivElement | null = $state(null);

  /** 当前打开的面板。一次只有一个。 */
  let sheet = $state<
    { kind: 'reasoning'; part: { text: string } } | { kind: 'tool'; msg: Message; tool: ToolPart } | null
  >(null);

  /**
   * 把工具调用压成一句人话。Claude 的中间步骤就是这样 ——
   * 一行截断描述，而不是把 JSON 摊在正文里。
   */
  function summarize(t: ToolPart): string {
    // 参数还在流式（JSON 没写完）时从半截文本里抠字段，边写边显示
    const a = (t.input ?? partialArgs(t.inputText)) as Record<string, unknown>;
    const s = (v: unknown) => (typeof v === 'string' ? v : '');
    // 只取第一行，长度交给 CSS 截断 —— 屏幕多宽就显示多少
    const firstLine = (v: string) => v.split('\n')[0].trim();

    switch (t.toolName) {
      case 'read':
        return `读取 ${s(a.path) || '文件'}`;
      case 'bash':
        return firstLine(s(a.command)) || '执行命令';
      case 'edit':
        return `编辑 ${s(a.path) || '文件'}`;
      default:
        return t.toolName || '工具调用';
    }
  }

  /**
   * 从还没写完的参数 JSON 里抠出已经出现的字段。
   *
   * 工具参数是一片一片流过来的（`{"command":"apk add py`…），等完整
   * JSON 才显示的话，模型写长命令那几秒工具行是空的。字符串字段取到
   * 目前为止的内容，数字字段写完了才取。
   */
  function partialArgs(text: string): Record<string, unknown> {
    if (!text) return {};
    try {
      return JSON.parse(text);
    } catch {
      /* 没写完，下面逐个抠 */
    }
    const out: Record<string, unknown> = {};
    for (const m of text.matchAll(/"(\w+)"\s*:\s*"((?:[^"\\]|\\.)*)/g)) {
      // 末尾可能是半个转义（`\`），去掉再解
      const raw = m[2].replace(/\\(u[0-9a-fA-F]{0,3})?$/, '');
      try {
        out[m[1]] = JSON.parse(`"${raw}"`);
      } catch {
        out[m[1]] = raw;
      }
    }
    for (const m of text.matchAll(/"(\w+)"\s*:\s*(-?\d+)\s*[,}]/g)) out[m[1]] = Number(m[2]);
    return out;
  }

  /** 面板标题。要短，细节放正文。 */
  function toolTitle(t: ToolPart): string {
    switch (t.toolName) {
      case 'read':
        return '读取文件';
      case 'bash':
        return '执行命令';
      case 'edit':
        return '编辑文件';
      default:
        return t.toolName || '工具调用';
    }
  }

  /** 面板正文的主体：能一眼看懂的那部分，而不是把 JSON 摊开 */
  function primary(t: ToolPart): string {
    const a = (t.input ?? partialArgs(t.inputText)) as Record<string, unknown>;
    const s = (v: unknown) => (typeof v === 'string' ? v : '');
    if (t.toolName === 'read') return s(a.path);
    if (t.toolName === 'bash') return s(a.command);
    if (t.toolName === 'edit') return s(a.path);
    if (t.input !== undefined) return JSON.stringify(t.input, null, 2);
    return t.inputText ?? '';
  }

  const asText = (v: unknown) => (typeof v === 'string' ? v : JSON.stringify(v, null, 2));

  /** 兜底：对象排成 key: value，去掉 JSON 的括号和引号噪音 */
  function kv(v: unknown): string {
    if (typeof v !== 'object' || v === null) return asText(v);
    return Object.entries(v as Record<string, unknown>)
      .map(([k, val]) => `${k}: ${typeof val === 'string' ? val : JSON.stringify(val)}`)
      .join('\n');
  }

  type Chip = { text: string; tone: 'ok' | 'bad' | 'warn' | 'plain' };

  type Result = {
    chips: Chip[];
    label: string;
    failed: boolean;
    body: string;
    diff: boolean;
    path: string | null;
    /** 工具返回的图片在沙箱里的路径（read 读图才有） */
    image: string | null;
  };

  /**
   * 把工具结果拆成能分样式显示的部分。
   *
   * 之前是把整个结果 JSON 摊开 —— output 里的换行会转义成字面量 `\n`，
   * 一屏全是反斜杠。现在每个字段有自己的样式：
   *   exit_code / wall_time / truncated / message → 顶部小药丸
   *   output  → 正文（非 0 退出时整块红）
   *   diff    → 按行染色（+ 绿 / - 红）
   *   error   → 整块红
   *   full_output_path → 底部的路径说明
   */
  function result(t: ToolPart): Result | null {
    const o = t.output;
    if (o === undefined) return null;

    const plain = (label: string, body: string, failed = false): Result => ({
      chips: [],
      label,
      failed,
      body,
      diff: false,
      path: null,
      image: null,
    });

    if (typeof o === 'string') return plain('输出', o);
    if (typeof o !== 'object' || o === null) return plain('输出', asText(o));

    const r = o as Record<string, unknown>;

    // 工具直接报错
    if (typeof r.error === 'string') return plain('错误', r.error, true);

    const chips: Chip[] = [];

    // bash 还在跑：服务端推的阶段性输出
    if (r.running === true) chips.push({ text: '运行中', tone: 'warn' });

    // bash
    if (typeof r.exit_code === 'number') {
      const ok = r.exit_code === 0;
      chips.push({ text: ok ? 'exit 0' : `exit ${r.exit_code}`, tone: ok ? 'plain' : 'bad' });
    }
    if (typeof r.wall_time_seconds === 'number') {
      chips.push({ text: `${r.wall_time_seconds}s`, tone: 'plain' });
    }
    if (r.truncated === true) chips.push({ text: '已截断', tone: 'warn' });

    // edit
    if (typeof r.message === 'string') chips.push({ text: r.message, tone: 'plain' });
    if (typeof r.firstChangedLine === 'number') {
      chips.push({ text: `第 ${r.firstChangedLine} 行起`, tone: 'plain' });
    }

    const failed = typeof r.exit_code === 'number' && r.exit_code !== 0;

    const image = typeof r.imagePath === 'string' ? r.imagePath : null;

    if (typeof r.diff === 'string') {
      return { chips, label: '改动', failed: false, body: r.diff, diff: true, path: null, image };
    }

    if (typeof r.output === 'string') {
      return {
        chips,
        label: '输出',
        failed,
        body: r.output,
        diff: false,
        path: typeof r.full_output_path === 'string' ? r.full_output_path : null,
        image,
      };
    }

    return { chips, label: '结果', failed: false, body: kv(o), diff: false, path: null, image };
  }

  /**
   * 输出在长的时候跟到最底下，像终端一样。
   *
   * 只在"本来就在底部"时跟：用户往上翻着看前面的输出，就别把他拽回去。
   */
  function followTail(node: HTMLElement) {
    const sc = node.closest('.overflow-y-auto') as HTMLElement | null;
    if (!sc) return;
    const gap = () => sc.scrollHeight - sc.scrollTop - sc.clientHeight;
    let atBottom = gap() < 40;
    const onScroll = () => (atBottom = gap() < 40);
    const mo = new MutationObserver(() => {
      if (atBottom) sc.scrollTop = sc.scrollHeight;
    });
    sc.addEventListener('scroll', onScroll, { passive: true });
    mo.observe(node, { childList: true, characterData: true, subtree: true });
    return {
      destroy() {
        mo.disconnect();
        sc.removeEventListener('scroll', onScroll);
      },
    };
  }

  /** diff 按行染色 */
  const diffLine = (l: string) => {
    if (l.startsWith('+') && !l.startsWith('+++')) return 'text-ok';
    if (l.startsWith('-') && !l.startsWith('---')) return 'text-bad';
    if (l.startsWith('@@') || l.startsWith('+++') || l.startsWith('---')) return 'text-faint';
    return 'text-muted';
  };

  const CHIP: Record<Chip['tone'], string> = {
    ok: 'bg-ok/15 text-ok',
    bad: 'bg-bad/15 text-bad',
    warn: 'bg-warn/15 text-warn',
    plain: 'bg-control text-muted',
  };

  // —— 贴底 ——
  //
  // 原来是监听 messages.length / parts.length 再滚到底，两个问题：
  //   1. 流式输出时文字在同一个 part 里变长，长度不变 → 不跟
  //   2. 用户往上翻着看，来一个新 part 就被硬拽回底部
  // 现在：在底部附近才算"贴着"；内容或容器尺寸一变（流式长高、键盘弹起、
  // 输入框长高），贴着就跟到底。用 ResizeObserver，不用每个 delta 都跑。
  let content: HTMLDivElement | null = $state(null);
  let pinned = true;

  const toBottom = () => {
    if (scroller) scroller.scrollTop = scroller.scrollHeight;
  };

  let lastTop = 0;

  // —— 窗口化 ——
  //
  // 长对话不能把几百条消息全挂进 DOM（节点数和内存都线性涨）。只渲染最后
  // `limit` 条；往上翻到顶再往前补一窗，并补偿 scrollTop，视口不跳。
  // 注意 `content-visibility: auto` 只省布局/绘制，**不省 DOM 节点** ——
  // 这个才是真的封顶。
  //
  // 这里的「条」是**消息**（用户/助手），不是工具调用：一个回合的思考、
  // 工具、正文都是同一条助手消息里的 parts。所以 40 条 ≈ 20 个回合，
  // 每个回合里几十个工具调用都算在这一条里。
  const WINDOW_STEP = 40;
  let limit = $state(WINDOW_STEP);
  let loadingEarlier = false;

  let start = $derived(Math.max(0, messages.length - limit));

  /** 切会话 → 窗口收回来（否则新会话只显示尾巴那几条） */
  let windowKey = $state('');
  $effect(() => {
    const k = messages[0]?.id ?? '';
    if (k !== windowKey) {
      windowKey = k;
      limit = WINDOW_STEP;
    }
  });

  async function loadEarlier() {
    if (loadingEarlier || start === 0 || !scroller) return;
    loadingEarlier = true;
    const before = scroller.scrollHeight;
    const top = scroller.scrollTop;
    limit += WINDOW_STEP;
    await tick();
    // 前面补进来多少高度，scrollTop 就补多少 —— 视口里那条不动
    scroller.scrollTop = top + (scroller.scrollHeight - before);
    lastTop = scroller.scrollTop;
    loadingEarlier = false;
  }

  /**
   * 只有用户**往上**翻才取消贴底。
   *
   * 原来是每次滚动都按"离底部多远"重算：程序刚把列表滚到底，滚动事件
   * 还没派发，内容又长了一截（流式时很常见），事件里一算离底部超过
   * 48px，就误判成用户往上翻了 —— 之后再也不跟，回复结束时停在半截。
   */
  function onscroll() {
    const el = scroller!;
    const gap = el.scrollHeight - el.scrollTop - el.clientHeight;
    if (gap < 48) pinned = true;
    else if (el.scrollTop < lastTop - 1) pinned = false;
    lastTop = el.scrollTop;
    // 快到顶了 → 再补一窗更早的（窗口化）
    if (el.scrollTop < 200) void loadEarlier();
  }

  $effect(() => {
    if (!scroller || !content) return;
    const ro = new ResizeObserver(() => pinned && toBottom());
    ro.observe(scroller);
    ro.observe(content);
    return () => ro.disconnect();
  });

  // 自己发了消息 / 切了会话 → 无论在哪都回到底部
  $effect(() => {
    void messages.length;
    void messages[0]?.id;
    pinned = true;
    toBottom();
  });
</script>

<!-- 底部留白 = 输入区高度 + 一点余量：输入区是浮在上面的，
     不留够的话最后一条消息永远有半截压在卡片下面。 -->
<div
  bind:this={scroller}
  {onscroll}
  class="h-full overflow-y-auto overscroll-y-contain px-4 pt-[calc(var(--hdr)+var(--fade-top))] pb-[calc(var(--composer-h,0px)+1rem)]"
>
<div bind:this={content} class="flex flex-col gap-6">
  {#if start > 0}
    <button
      onclick={loadEarlier}
      class="self-center rounded-full bg-hairline px-3 py-1 text-xs text-muted"
    >
      显示更早的 {Math.min(WINDOW_STEP, start)} 条
    </button>
  {/if}
  {#each messages.slice(start) as msg (msg.id)}
    {#if msg.role === 'user'}
      {@const atts = msg.parts.filter((p) => p.type === 'image' || p.type === 'file')}
      {@const text = msg.parts.map((p) => (p.type === 'text' ? p.text : '')).join('')}
      {#if atts.length === 0}
        <!-- 常见情况：只有正文，一层 <p> 就够。不为了「可能会有的附件」
             恒常多套一层 div（对齐 + 收缩 + 上限 84%）。 -->
        <p
          class="msg self-end max-w-[84%] rounded-card bg-bubble px-3.5 py-2.5 text-base leading-6 whitespace-pre-wrap break-words"
        >
          {text}
        </p>
      {:else}
        <!-- 有附件：缩略图/药丸在上，正文气泡在下 -->
        <div class="msg flex max-w-[84%] flex-col items-end gap-2 self-end">
          <div class="flex flex-wrap justify-end gap-2">
            {#each atts as a (a.id)}
              {#if a.type === 'image'}
                <img
                  src={a.url}
                  alt={a.name}
                  class="max-h-48 max-w-full rounded-card border border-hairline object-cover"
                />
              {:else}
                <span
                  class="flex items-center gap-1.5 rounded-card bg-bubble px-3 py-2 text-meta text-fg"
                >
                  <Icon name="file" size={16} class="shrink-0 text-muted" />
                  <span class="max-w-52 truncate">{a.name}</span>
                </span>
              {/if}
            {/each}
          </div>
          {#if text}
            <p
              class="max-w-full rounded-card bg-bubble px-3.5 py-2.5 text-base leading-6 whitespace-pre-wrap break-words"
            >
              {text}
            </p>
          {/if}
        </div>
      {/if}
    {:else}
      <div class="flex flex-col gap-2.5 text-base leading-relaxed {msg.done ? 'msg' : ''}">
        {#each msg.parts as part, i (i)}
          {#if part.type === 'text'}
            <Markdown text={part.text} />
          {:else if part.type === 'reasoning'}
            <!-- 一行：它此刻在想什么。点开面板看全文 -->
            {@const live = !msg.done && i === msg.parts.length - 1}
            <button
              onclick={() => (sheet = { kind: 'reasoning', part })}
              class="flex w-full items-center gap-2 px-2.5 py-0.5 text-left"
            >
              <Icon name="sparkle" size={16} class="shrink-0 text-faint" />
              <ThoughtLine text={part.text} {live} />
              <Icon name="chevron" size={15} class="ml-auto shrink-0 text-faint" />
            </button>
          {:else if part.type === 'tool'}
            {@const tool = part}
            <button
              onclick={() => (sheet = { kind: 'tool', msg, tool })}
              class="flex w-full items-center gap-2 px-2.5 py-0.5 text-left"
            >
              <Icon name="clock" size={16} class="shrink-0 text-faint" />
              <span class="truncate text-sm text-muted">{summarize(tool)}</span>
              {#if tool.state !== 'output' && !msg.done}
                <span class="shrink-0 text-meta text-faint">运行中</span>
              {/if}
              <Icon name="chevron" size={15} class="ml-auto shrink-0 text-faint" />
            </button>
          {:else if part.type === 'data'}
            <!-- 用量不显示（Claude 没有，看着啰嗦）；警告要显示，
                 否则「输出被长度上限截断」这种中途停下的提示是隐形的 -->
            {#if part.name === 'warning'}
              {@const w = part.data as { message?: string }}
              {#if w?.message}
                <p class="text-[13px] text-warn">⚠️ {w.message}</p>
              {/if}
            {/if}
          {:else if part.type === 'error'}
            <p class="text-[13px] text-bad">错误：{part.text}</p>
          {/if}
        {/each}

      </div>
    {/if}
  {/each}
</div>
</div>

<style>
  /* 屏幕外的消息跳过布局和绘制。长对话里这是划动掉帧的大头。
     auto 让浏览器记住渲染过的真实高度，翻回来不会跳。
     正在流式的那条不加 —— 它一直在变。 */
  .msg {
    content-visibility: auto;
    contain-intrinsic-size: auto 6rem;
  }
</style>

{#if sheet}
  {#if sheet.kind === 'reasoning'}
    <Sheet title="思考" onclose={() => (sheet = null)}>
      <!-- 存的是 part 本身而不是文本副本：思考还在流式时打开，面板里也跟着长 -->
      <div class="text-[15px] leading-6 text-fg/90"><Markdown text={sheet.part.text} /></div>
    </Sheet>
  {:else}
    <!-- 存成 const：sheet 是 $state，在 onclick 闭包里 TS 收窄不了 -->
    {@const s = sheet}
    {@const t = s.tool}
    {@const res = result(t)}
    <Sheet title={toolTitle(t)} onclose={() => (sheet = null)}>
      <div class="flex flex-col gap-4">
        {#if primary(t)}
          <pre
            class="rounded-xl bg-hairline-soft px-3 py-2.5 font-mono text-[13px] leading-5 whitespace-pre-wrap break-all">{primary(
              t,
            )}</pre>
        {/if}

        {#if res}
          <div class="flex flex-col gap-2">
            <!-- 元信息药丸：退出码 / 耗时 / 是否截断 / 替换处数 -->
            <div class="flex flex-wrap items-center gap-1.5">
              <span class="text-meta {res.failed ? 'text-bad' : 'text-faint'}">{res.label}</span>
              {#each res.chips as c}
                <span class="rounded-full px-2 py-0.5 text-meta {CHIP[c.tone]}">{c.text}</span>
              {/each}
            </div>

            {#if res.image}
              <!-- read 读到的图：直接展示原图（沙箱路由），模型看的是缩放后的那份 -->
              <img
                src={`/sandbox${encodeURI(res.image)}`}
                alt="工具返回的图片"
                class="max-h-80 w-auto self-start rounded-xl bg-hairline-soft object-contain"
              />
            {/if}

            {#if res.diff}
              <div
                class="overflow-x-auto rounded-xl bg-hairline-soft px-3 py-2.5 font-mono text-[13px] leading-5"
              >
                {#each res.body.split('\n') as line}
                  <p class="whitespace-pre {diffLine(line)}">{line || ' '}</p>
                {/each}
              </div>
            {:else}
              <pre
                use:followTail
                class="font-mono text-[13px] leading-5 whitespace-pre-wrap break-all {res.failed
                  ? 'text-bad'
                  : ''}">{res.body}</pre>
            {/if}

            {#if res.path}
              <p class="text-meta text-faint">
                完整输出：<code class="font-mono">{res.path}</code>
              </p>
            {/if}
          </div>
        {:else if t.state === 'streaming'}
          <p class="text-sm text-muted">执行中…</p>
        {/if}
      </div>
    </Sheet>
  {/if}
{/if}
