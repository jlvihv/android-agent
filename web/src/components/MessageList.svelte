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
  import { BROWSER_ACTION, describeInput, describeResult, failed, type Tone } from '../lib/toolview';
  import Icon from './Icon.svelte';
  import { openViewer } from '../lib/viewer.svelte.ts';
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
    /** 把排队的那条从队列里去掉（点那个 ×） */
    ondrop,
    /**
     * 「我自己刚发了一条」的次数（App 数）。变了就把视口带回尾部。
     *
     * 为什么不能盯 `messages` 自己：服务端往列表里加东西（排队那条被交给模型、
     * 轮询补历史）也会让 `messages` 变 —— 那都是**别人在动**，用户可能正往上翻着
     * 看，一拽就回到尾部。只有这一个回调代表用户自己的动作。
     */
    follow = 0,
    /** 当前会话 id：变了（切会话 / 第一次装载）就把视口落在尾部 */
    session = '',
  }: {
    messages: Message[];
    pending?: Message[];
    /** 把排队的那条从队列里去掉（点那个 ×） */
    ondrop?: (id: string) => void;
    follow?: number;
    session?: string;
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
    // 每来一小段参数这里就重算一次：对**整段** inputText 跑正则 + 试
    // JSON.parse —— 模型用 edit 写一个 30KB 的文件，就是几千次 × 30KB 的反复
    // 扫描，手机上肉眼可见地卡。
    const a = summaryArgs(t);
    const s = (v: unknown) => (typeof v === 'string' ? v : '');
    // 只取第一行，长度交给 CSS 截断 —— 屏幕多宽就显示多少
    const firstLine = (v: string) => v.split('\n')[0].trim();

    switch (t.toolName) {
      case 'read':
        return `读取 ${s(a.path) || '文件'}`;
      case 'bash':
        return firstLine(s(a.command)) || '执行命令';
      case 'edit':
        // 新建还是改写：核心用 `content` / `edits` 两个参数区分（见 core 的
        // edit_mode），这里认同一个信号 —— 两个都给是报错的用法，认 `content`
        // 就行（那一行也会是红的）。摘要里也分两种说法，跟图标对得上。
        return isNewFile(a) ? `新建 ${s(a.path) || '文件'}` : `编辑 ${s(a.path) || '文件'}`;
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
   * 折叠行用的参数。
   *
   * **只看开头那一段**（见 `summarize` 里那段注释）：这一行只要命令 / 路径 /
   * `content` 这种排在前面的短字段，而且每来一小段参数就会重算一次 —— 对整段
   * inputText 跑，模型用 edit 写一个 30KB 的文件就是几千次 × 30KB。
   * 新建文件的那个 `content` 就在开头（path 只有几个字符），这一截里照样认得出。
   */
  const summaryArgs = (t: ToolPart) =>
    (t.input ?? partialArgs(t.inputText.slice(0, SUMMARY_SCAN))) as Record<string, unknown>;

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
  function toolTitle(t: ToolPart, args: Record<string, unknown>): string {
    switch (t.toolName) {
      case 'read':
        return '读取文件';
      case 'bash':
        return '执行命令';
      case 'edit':
        return isNewFile(args) ? '新建文件' : '编辑文件';
      case 'browser':
        return '浏览器';
      case 'search':
        // 同 browser：老会话回放用（`search` 工具已删）
        return '联网搜索';
      default:
        return t.toolName || '工具调用';
    }
  }

  /** `edit` 这次是新建文件（`content`）还是改已有文件（`edits`） */
  const isNewFile = (a: Record<string, unknown>) => a.content !== undefined;

  /** 这几个图标名字得和 Icon.svelte 里的一致（写错了下面会编译报错） */
  type ToolIcon = 'file' | 'newfile' | 'terminal' | 'pencil' | 'globe' | 'search' | 'clock';

  /**
   * 工具那一行左边的小图标：一眼能看出这次是在读、在跑命令、还是在改文件。
   *
   * 不认识的工具仍然给时钟：以后核心加了新工具，是「多了个没见过的图标」
   * 而不是「这一行空了」。类型写成 `ToolIcon`（不是 `string`）—— 图标改名的话
   * 这里会编译报错，不会静静地画不出东西。
   */
  const TOOL_ICON: Record<string, ToolIcon> = {
    read: 'file',
    bash: 'terminal',
    edit: 'pencil',
    // browser / search 这两个工具已删，只剩回放老会话（见 summarize）
    browser: 'globe',
    search: 'search',
  };
  function toolIcon(t: ToolPart): ToolIcon {
    if (t.toolName === 'edit') return isNewFile(summaryArgs(t)) ? 'newfile' : 'pencil';
    return TOOL_ICON[t.toolName] ?? 'clock';
  }

  /** 参数还在流式时从半截 JSON 里抠（见 partialArgs） */
  const argsOf = (t: ToolPart) => (t.input ?? partialArgs(t.inputText)) as Record<string, unknown>;

  /**
   * 面板里那份参数。面板要**完整**的参数（不能像折叠行那样只看开头），而参数还在
   * 流式时每来一小段就整段解析一遍 —— 模型用 edit 写一个 30KB 的文件就是几千次 ×
   * 30KB。和 Markdown 同一个办法：变化先攒着，一帧解析一次。打开面板那一下在
   * `openTool` 里同步算好，第一帧就对。
   */
  let panelArgs = $state<Record<string, unknown>>({});
  let argsRaf = 0;
  $effect(() => {
    const t = sheet?.kind === 'tool' ? sheet.tool : null;
    if (!t) return;
    if (t.input !== undefined) {
      panelArgs = t.input as Record<string, unknown>;
      return;
    }
    void t.inputText;
    argsRaf ||= requestAnimationFrame(() => {
      argsRaf = 0;
      panelArgs = argsOf(t);
    });
  });
  $effect(() => () => cancelAnimationFrame(argsRaf));

  function openTool(msg: Message, tool: ToolPart) {
    panelArgs = argsOf(tool);
    sheet = { kind: 'tool', msg, tool };
  }

  /**
   * 「离底部多近算贴着底」（px）。两处用同一个数：列表的贴底判定（`onscroll`）
   * 和工具输出里的跟尾（`followTail`）—— 同一个含义别写两个值。
   *
   * 不能太小：流式时内容刚长高一截、滚动事件还没派发，一算就超过阈值，会把
   * 「程序刚滚到底」误判成「用户往上翻了」，之后再也不会跟。
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
  // 监听 messages.length / parts.length 再滚到底有两个问题：
  //   1. 流式输出时文字在同一个 part 里变长，长度不变 → 不跟
  //   2. 用户往上翻着看，来一个新 part 就被硬拽回底部
  // 现在：在底部附近才算"贴着"；内容或容器尺寸一变（流式长高、键盘弹起、
  // 输入框长高），贴着就跟到底。用 ResizeObserver，不用每个 delta 都跑。
  let content: HTMLDivElement | null = $state(null);
  let pinned = $state(true);
  /** 离底部多远（px）。除了 `pinned` 还要它：决定要不要显「回到最新」 */
  let gap = $state(0);

  /**
   * 「回到最新」的显示门槛（px）。
   *
   * 比 `NEAR_BOTTOM`（48，判贴底）大一截：48 那块是「差不多就在底部」的
   * 缓冲区，一进去按钮就冒出来会显得毛躁。
   */
  const JUMP_GAP = 120;
  let showJump = $derived(!pinned && gap > JUMP_GAP);

  const toBottom = () => {
    if (!scroller) return;
    scroller.scrollTop = scroller.scrollHeight;
    // 到底了就是 0：不写的话按钮要等下一个滚动事件才收起来
    gap = 0;
  };

  /** 点「回到最新」：跟用户自己滚到底一个效果 */
  const jumpToLatest = () => {
    pinned = true;
    toBottom();
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
    if (loadingEarlier || start === 0 || !scroller || !content) return;
    loadingEarlier = true;

    // 锚定当前视口里**最上面那条消息**，补完把它对回原处。
    //
    // 为什么不用「补进来多少高度就补多少 scrollTop」（以前就是那么干的）：
    // 那个写法默认「这期间容器高度只因为补窗变过」—— 锚点法不依赖任何前提，
    // 只认那一条消息自己的位置（只有**它上面**多出来的东西会移动它）。
    //
    // （实测两种写法在夹具里都是 0px：`await tick()` 只跨一个微任务，中间
    // 几乎长不出东西。留着是为了前提变化时不至于露出来 —— 比如以后把
    // `tick()` 换成等一帧。）
    //
    // 「显示更早的 N 条」按钮要排除（它在消息前面，自己的位置不会变）——
    // 拿它当锚点就等于不补偿，正在读的那条会被顶下去。
    const kids = [...content.children].filter((el) => !el.classList.contains('load-earlier'));
    const edge = scroller.getBoundingClientRect().top;
    const anchor = kids.find((el) => el.getBoundingClientRect().bottom > edge);
    const before = anchor?.getBoundingClientRect().top ?? 0;

    limit += WINDOW_STEP;
    await tick();

    if (anchor) scroller.scrollTop += anchor.getBoundingClientRect().top - before;
    lastTop = scroller.scrollTop;
    loadingEarlier = false;
  }

  /**
   * 只有用户**往上**翻才取消贴底。
   *
   * 每次滚动都按"离底部多远"重算的话：程序刚把列表滚到底，滚动事件
   * 还没派发，内容又长了一截（流式时很常见），事件里一算离底部超过
   * 48px，就误判成用户往上翻了 —— 之后再也不跟，回复结束时停在半截。
   */
  function onscroll() {
    const el = scroller!;
    gap = el.scrollHeight - el.scrollTop - el.clientHeight;
    if (gap < NEAR_BOTTOM) pinned = true;
    else if (el.scrollTop < lastTop - 1) pinned = false;
    lastTop = el.scrollTop;
    // 快到顶了 → 再补一窗更早的（窗口化）
    if (el.scrollTop < LOAD_EARLIER_AT) void loadEarlier();
  }

  $effect(() => {
    if (!scroller || !content) return;
    const ro = new ResizeObserver(() => {
      // 离底部的距离也跟着内容长：不更新的话，用户在往上翻时
      // 「回到最新」要靠下一次滚动才冒出来
      gap = scroller!.scrollHeight - scroller!.scrollTop - scroller!.clientHeight;
      if (pinned) toBottom();
    });
    ro.observe(scroller);
    ro.observe(content);
    return () => ro.disconnect();
  });

  // —— 为什么不给消息加 `content-visibility: auto` ——
  //
  // 给「已经说完的」消息加 `content-visibility: auto` + `contain-
  // intrinsic-size: auto 6rem` 能省离屏消息的布局/绘制，但**加上类的那一帧**
  // 会先把元素按 6rem 量一遍：回复很长时（它露在视口里的只是一小截），浏览器
  // 判定这条「对用户不重要」，整条被跳过、塌成 96px，滚动容器的 scrollHeight
  // 跟着缩，scrollTop 被夹到新上限 —— 视口就跳回这条消息的**开头**。
  // 高度下一帧会恢复，**夹掉的 scrollTop 不会**。数字：
  //
  //   加类前 scrollTop=12371 scrollHeight=12956
  //   加类后 scrollTop=1238  scrollHeight=1838   ← 跳回消息开头
  //
  // 而「说完话」正好就是 `msg.done` 翻 true、这条消息拿到类的那一刻，所以
  // 每次回复结束都跳一次。省下的那点绘制不值这个 bug —— DOM 本来就只挂
  // 最后 40 条（见下面的窗口化），封顶靠它，不靠 content-visibility。

  // 自己发了消息 / 切了会话 → 回到尾部。
  //
  // **只认这两件事**，不能盯 `messages` 的长度：排队那条被交给模型
  //（`data-queued-delivered` → App 里 `messages.push`）、轮询把服务端的新历史换上
  //（`loadMessages`）都会动这个数组，而那是服务端在动 —— 用户正往上翻着看的时候，
  // 一拽就回到尾部。实测：流式里往上翻着读，紧接着第二句被交给模型的那一刻，
  // 视口从 top=0 直接被写到最底（调用栈就落在下面这两行上）。
  $effect(() => {
    void follow;
    pinned = true;
    toBottom();
  });

  let lastSession = $state('');
  $effect(() => {
    if (session === lastSession) return;
    lastSession = session;
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
            <!-- 点开看大图：和回复里的图片同一个查看器（Viewer.svelte） -->
            <button
              type="button"
              onclick={() => openViewer(a.url, a.name, 'image')}
              aria-label="查看大图：{a.name}"
              class="block max-w-full overflow-hidden rounded-card border border-hairline"
            >
              <img src={a.url} alt={a.name} class="block max-h-48 max-w-full object-cover" />
            </button>
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
      class="load-earlier self-center rounded-full bg-hairline px-3 py-1 text-xs text-muted"
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
            <!-- 失败的行整行染红（图标 + 摘要）：16px 的淡灰图标单独变红，
                 在一列工具行里扫不出来；面板里失败的标题和正文本来也是红的。
                 右边的 › 不跟着红 —— 它是“点得开”，跟成功失败无关。
                 判定走 toolview 的 `failed()`，和面板同一个函数。 -->
            {@const bad = failed(tool)}
            <button
              onclick={() => openTool(msg, tool)}
              class="flex w-full items-center gap-2 px-2.5 py-0.5 text-left"
            >
              <Icon name={toolIcon(tool)} size={16} class="shrink-0 {bad ? 'text-bad' : 'text-faint'}" />
              <span class="truncate text-sm {bad ? 'text-bad' : 'text-muted'}">{summarize(tool)}</span>
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

<!-- 「回到最新」：往上翻着看历史时，输出还在往下长 —— 给一个一步回到尾部的
     入口（没有它就只能一路划回去，或者等下一轮把它顶走）。

     尺寸 / 圆角 / 图标字号跟输入卡片里那两个圆按钮**一模一样**
     （`size-9` + `rounded-full` + 18px 图标）—— 同一个 App 里同一类控件不该有两种大小。
     只有图标没有文字（`aria-label` / `title` 顶上），就跟停止键一样。

     `absolute` 挂在 App 的 `main`（`absolute inset-0`）上，所以宽屏有侧边栏
     时它跟着**对话区**居中，不是整个窗口居中。底部避让用跟滚动容器 padding
     同一个值（`--composer-h`）—— 输入卡片的高度一变，两边一起动。 -->
{#if showJump}
  <button
    type="button"
    onclick={jumpToLatest}
    aria-label="回到最新"
    title="回到最新"
    class="absolute bottom-[calc(var(--composer-h,0px)+1.25rem)] left-1/2 z-10 grid
           size-9 -translate-x-1/2 place-items-center rounded-full bg-control text-fg
           ring-1 ring-hairline"
  >
    <Icon name="down" size={18} />
  </button>
{/if}

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
    {@const inp = describeInput(t, panelArgs)}
    <Sheet title={toolTitle(t, panelArgs)} onclose={() => (sheet = null)}>
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
