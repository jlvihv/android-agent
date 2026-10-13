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
  import { sandboxPath } from '../lib/markdown';
  import { BROWSER_ACTION, describeInput, describeResult, type Tone } from '../lib/toolview';
  import Icon from './Icon.svelte';
  import Markdown from './Markdown.svelte';
  import Sheet from './Sheet.svelte';
  import ThoughtLine from './ThoughtLine.svelte';

  let {
    messages,
    /**
     * 排队中（还没发出去）的用户消息。
     *
     * 走**和已经发出去的那条一模一样的气泡**（同一个 snippet、同一套尺寸/
     * 颜色/对齐），只多一圈虚线和一个 ×。`done: false` 就是这个意思 —— 正文
     * 里的用户消息永远是 `true`。
     */
    pending = [],
    ondrop,
  }: {
    messages: Message[];
    pending?: Message[];
    /** 把排队的那条从队列里去掉（点那个 ×） */
    ondrop?: (id: string) => void;
  } = $props();

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
    // 参数还在流式（JSON 没写完）时从半截文本里抠字段，边写边显示。
    //
    // 只看开头那一段：这一行只要命令 / 路径 / 网址这种排在前面的短字段。
    // 每来一小段参数这里就重算一次，原来是对**整段** inputText 跑正则 + 试
    // JSON.parse —— 模型用 edit 写一个 30KB 的文件，就是几千次 × 30KB 的反复
    // 扫描，手机上肉眼可见地卡。
    const a = (t.input ?? partialArgs(t.inputText.slice(0, SUMMARY_SCAN))) as Record<string, unknown>;
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
      case 'browser':
        // 浏览器工具已经删了（唯一入口是沙箱里的 `agent browser`，动作输出长
        // 在 bash 卡片里）。这里留着只为**回放老会话** —— 历史里那些 browser
        // 工具调用得照旧显示成人话。
        return `浏览器 ${browserSummary(a)}`;
      case 'search':
        // 同 browser：`search` 工具也删了（唯一入口是沙箱里的 `agent search`，
        // 结果长在 bash 卡片里）。这里只为回放老会话。
        return `搜索 ${firstLine(s(a.query))}`.trim();
      default:
        return t.toolName || '工具调用';
    }
  }

  /** 折叠行的摘要从流式参数里最多看这么多字符（见 summarize） */
  const SUMMARY_SCAN = 2048;

  /**
   * 浏览器那一行：动作 + 目标。
   *
   * 目标按动作取一个（url / selector / x,y）—— 这一行是折叠状态下**唯一**
   * 能看到的东西，把 action 和它的对象放在一起才有信息量。
   */
  function browserSummary(a: Record<string, unknown>): string {
    const s = (v: unknown) => (typeof v === 'string' ? v : '');
    const action = s(a.action) || '?';
    const label = BROWSER_ACTION;
    const target = s(a.url) || s(a.selector) || (a.x !== undefined ? `${a.x},${a.y}` : '');
    const one = target.split('\n')[0].trim();
    // 多标签时把"对哪只标签"带上 —— 同一行里 action 和对象要够判断
    const tab = a.tab !== undefined ? `#${a.tab} ` : '';
    return `${tab}${label[action] ?? action} ${one}`.trim();
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
      case 'browser':
        return '浏览器';
      case 'search':
        // 同 browser：老会话回放用（`search` 工具已删）
        return '联网搜索';
      default:
        return t.toolName || '工具调用';
    }
  }

  /** 参数还在流式时从半截 JSON 里抠（见 partialArgs） */
  const argsOf = (t: ToolPart) => (t.input ?? partialArgs(t.inputText)) as Record<string, unknown>;

  /**
   * 「离底部多近算贴着底」（px）。两处用同一个数：列表的贴底判定（`onscroll`）
   * 和工具输出里的跟尾（`followTail`）—— 同一个含义别写两个值。
   *
   * 不能太小：流式时内容刚长高一截、滚动事件还没派发，一算就超过阈值，会把
   * 「程序刚滚到底」误判成「用户往上翻了」，之后再也不会跟（踩过）。
   */
  const NEAR_BOTTOM = 48;

  /**
   * 输出在长的时候跟到最底下，像终端一样。
   *
   * 只在"本来就在底部"时跟：用户往上翻着看前面的输出，就别把他拽回去。
   */
  function followTail(node: HTMLElement) {
    const sc = node.closest('.overflow-y-auto') as HTMLElement | null;
    if (!sc) return;
    const gap = () => sc.scrollHeight - sc.scrollTop - sc.clientHeight;
    let atBottom = gap() < NEAR_BOTTOM;
    const onScroll = () => (atBottom = gap() < NEAR_BOTTOM);
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

  const CHIP: Record<Tone, string> = {
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
  /** 滚到这个位置（px）就再补一窗更早的 */
  const LOAD_EARLIER_AT = 200;
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
    if (gap < NEAR_BOTTOM) pinned = true;
    else if (el.scrollTop < lastTop - 1) pinned = false;
    lastTop = el.scrollTop;
    // 快到顶了 → 再补一窗更早的（窗口化）
    if (el.scrollTop < LOAD_EARLIER_AT) void loadEarlier();
  }

  $effect(() => {
    if (!scroller || !content) return;
    const ro = new ResizeObserver(() => pinned && toBottom());
    ro.observe(scroller);
    ro.observe(content);
    return () => ro.disconnect();
  });

  // —— 为什么不给消息加 `content-visibility: auto` ——
  //
  // 这里原来给「已经说完的」消息加 `content-visibility: auto` + `contain-
  // intrinsic-size: auto 6rem`，省离屏消息的布局/绘制。问题是**加上类的那一帧**
  // 会先把元素按 6rem 量一遍：回复很长时（它露在视口里的只是一小截），浏览器
  // 判定这条「对用户不重要」，整条被跳过、塌成 96px，滚动容器的 scrollHeight
  // 跟着缩，scrollTop 被夹到新上限 —— 视口就跳回这条消息的**开头**。
  // 高度下一帧会恢复，**夹掉的 scrollTop 不会**。实测：
  //
  //   加类前 scrollTop=12371 scrollHeight=12956
  //   加类后 scrollTop=1238  scrollHeight=1838   ← 跳回消息开头
  //
  // 而「说完话」正好就是 `msg.done` 翻 true、这条消息拿到类的那一刻，所以
  // 每次回复结束都跳一次。省下的那点绘制不值这个 bug —— DOM 本来就只挂
  // 最后 40 条（见下面的窗口化），封顶靠它，不靠 content-visibility。

  // 自己发了消息 / 切了会话 → 无论在哪都回到底部
  $effect(() => {
    void messages.length;
    void messages[0]?.id;
    pinned = true;
    toBottom();
  });
</script>

<!-- 用户消息的气泡。**排队中那条也走这里** —— 同一个气泡、同一套尺寸、
     颜色、对齐，只多一圈虚线；× 挂在气泡**左边**（那个位置本来就是空的：
     用户消息贴右）。挂出去而不是挤进去，气泡的宽度就和发出去的一模一样。 -->
{#snippet userMessage(msg: Message)}
  {@const atts = msg.parts.filter((p) => p.type === 'image' || p.type === 'file')}
  {@const text = msg.parts.map((p) => (p.type === 'text' ? p.text : '')).join('')}
  {@const queued = !msg.done}
  {#if atts.length === 0}
    <!-- 常见情况：只有正文，一层 <p> 就够。不为了「可能会有的附件」
         恒常多套一层 div（对齐 + 收缩 + 上限 84%）。 -->
    <p
      class="self-end max-w-[84%] rounded-card px-3.5 py-2.5 text-base leading-6 whitespace-pre-wrap break-words
             {queued ? 'relative border border-dashed border-control bg-bubble/40' : 'bg-bubble'}"
    >
      {text}
      {#if queued}{@render dropBtn(msg.id)}{/if}
    </p>
  {:else}
    <!-- 有附件：缩略图/药丸在上，正文气泡在下 -->
    <div class="flex max-w-[84%] flex-col items-end gap-2 self-end {queued ? 'relative' : ''}">
      {#if queued}{@render dropBtn(msg.id)}{/if}
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
          class="max-w-full rounded-card px-3.5 py-2.5 text-base leading-6 whitespace-pre-wrap break-words
                 {queued ? 'border border-dashed border-control bg-bubble/40' : 'bg-bubble'}"
        >
          {text}
        </p>
      {/if}
    </div>
  {/if}
{/snippet}

{#snippet dropBtn(id: string)}
  <button
    onclick={() => ondrop?.(id)}
    aria-label="从队列里去掉"
    title="从队列里去掉"
    class="absolute top-1/2 -left-7 grid size-5 -translate-y-1/2 place-items-center rounded-full
           bg-ink text-fg ring-1 ring-hairline"
  >
    <Icon name="close" size={11} />
  </button>
{/snippet}

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
      {@render userMessage(msg)}
    {:else}
      <div class="flex flex-col gap-2.5 text-base leading-relaxed">
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

  <!-- 排队中（还没发出去）的那几条：画在正文末尾，就是普通用户气泡的样子，
       只是虚线 + 左边一个 ×。数据上它们不在 `messages` 里（没发过、服务端
       不知道），发出去的那一刻才变成真正的那条 —— 样子几乎不变，所以看起来
       就是「虚线变实」。 -->
  {#each pending as p (p.id)}
    {@render userMessage(p)}
  {/each}
</div>
</div>

{#snippet fields(title: string, list: { key: string; text: string; block: boolean }[])}
  <div class="flex flex-col gap-1.5">
    <span class="text-meta text-faint">{title}</span>
    {#each list as f}
      {#if f.block}
        <div class="flex flex-col gap-1">
          <span class="font-mono text-meta text-muted">{f.key}</span>
          <pre
            class="rounded-xl bg-hairline-soft px-3 py-2.5 font-mono text-[13px] leading-5 whitespace-pre-wrap break-all">{f.text}</pre>
        </div>
      {:else}
        <p class="font-mono text-[13px] leading-5 break-all">
          <span class="text-muted">{f.key}</span>
          {f.text}
        </p>
      {/if}
    {/each}
  </div>
{/snippet}

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
    {@const res = describeResult(t)}
    {@const inp = describeInput(t, argsOf(t))}
    <Sheet title={toolTitle(t)} onclose={() => (sheet = null)}>
      <div class="flex flex-col gap-4">
        {#if inp.primary}
          <pre
            class="rounded-xl bg-hairline-soft px-3 py-2.5 font-mono text-[13px] leading-5 whitespace-pre-wrap break-all">{inp.primary}</pre>
        {/if}
        {#if inp.params.length}
          <!-- 主行之外的参数（bash 的 timeout、read 的 offset/limit、edit 新建时的全文 …） -->
          {@render fields('参数', inp.params)}
        {/if}

        {#if res}
          <div class="flex flex-col gap-2">
            <!-- 元信息药丸：退出码 / 耗时 / 是否截断 / 替换处数 … -->
            <div class="flex flex-wrap items-center gap-1.5">
              <span class="text-meta {res.failed ? 'text-bad' : 'text-faint'}">{res.label}</span>
              {#each res.chips as c}
                <span class="rounded-full px-2 py-0.5 text-meta {CHIP[c.tone]}">{c.text}</span>
              {/each}
            </div>

            {#if res.image}
              <!-- 工具返回的图：直接展示原图（沙箱路由），模型看的是缩放后的那份 -->
              <img
                src={sandboxPath(res.image)}
                alt="工具返回的图片"
                class="max-h-80 w-auto self-start rounded-xl bg-hairline-soft object-contain"
              />
              <p class="text-meta text-faint">图片：<code class="font-mono break-all">{res.image}</code></p>
            {/if}

            {#if res.body !== null}
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
            {/if}

            {#if res.path}
              <p class="text-meta text-faint">
                完整输出：<code class="font-mono break-all">{res.path}</code>
              </p>
            {/if}

            {#if res.urls.length}
              <!-- 输出里挑出来的网址：模型拿到的是同一份清单 -->
              <div class="flex flex-col gap-1">
                <span class="text-meta text-faint">网址</span>
                {#each res.urls as u}
                  <a href={u} target="_blank" rel="noreferrer" class="font-mono text-[13px] break-all underline decoration-faint underline-offset-2"
                    >{u}</a
                  >
                {/each}
              </div>
            {/if}
          </div>

          {#if res.fields.length}
            <!-- 没有专门样式的字段：一个都不藏（模型看得到的，这里就得看得到） -->
            {@render fields('其他字段', res.fields)}
          {/if}

          {#if res.raw !== null}
            <details class="group">
              <summary class="cursor-default list-none text-meta text-faint">
                <span class="inline-block transition-transform group-open:rotate-90">›</span>
                模型收到的原始结果{res.image ? '（另附上面那张图，缩放后）' : ''}
              </summary>
              <pre
                class="mt-2 rounded-xl bg-hairline-soft px-3 py-2.5 font-mono text-[12px] leading-5 whitespace-pre-wrap break-all text-muted">{res.raw}</pre>
            </details>
          {/if}
        {:else if t.state === 'streaming'}
          <p class="text-sm text-muted">执行中…</p>
        {/if}
      </div>
    </Sheet>
  {/if}
{/if}
