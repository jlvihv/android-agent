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
  import { tick, untrack } from 'svelte';
  import Img from './Img.svelte';
  import { cubicIn } from 'svelte/easing';
  import type { Message, ToolPart } from '../lib/sse';
  import { sandboxPath } from '../lib/markdown';
  import { describeInput, describeResult, failed } from '../lib/toolview';
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
    /** 服务端还有没有更早的（`App` 说）—— 手上这一段到顶了就自己叫 `onloadearlier` 去要 */
    hasolder = false,
    /** 往上翻到头（或这一窗还不到一屏）时去服务端要一窗更早的。返回 true = 真的拿到了 */
    onloadearlier,
    /** 一窗多少条（和 `App` 发给服务端的 `tail` 同一个数） */
    step = 10,
  }: {
    messages: Message[];
    pending?: Message[];
    /** 把排队的那条从队列里去掉（点那个 ×） */
    ondrop?: (id: string) => void;
    follow?: number;
    session?: string;
    hasolder?: boolean;
    onloadearlier?: () => Promise<boolean>;
    step?: number;
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
      default:
        return t.toolName || '工具调用';
    }
  }

  /** `edit` 这次是新建文件（`content`）还是改已有文件（`edits`） */
  const isNewFile = (a: Record<string, unknown>) => a.content !== undefined;

  /** 这几个图标名字得和 Icon.svelte 里的一致（写错了下面会编译报错） */
  type ToolIcon = 'file' | 'newfile' | 'terminal' | 'pencil' | 'clock';

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

  /**
   * 「回到最新」那个光圈按钮的收放：200ms、`cubicIn`（**先慢后快**）。
   *
   * 缓动用的是 `1-(1-t)³` 的镜像 `t³`：前 1/5 时间只走 0.8%、最后 1/5 走 48.8% ——
   * 圈先摆在那儿几乎不动，最后一下阖进那个点（跟面板/抽屉那条“松手回位”
   * 的曲线刚好反向）。想回到先快后慢：`cubicIn` 换 `cubicOut`。
   *
   * 为什么不用 `svelte/transition` 的 `scale`：它把整个按钮（连里头的箭头）
   * 一起缩，圈变小的时候箭头也在变小。这里要的是**圈变小、箭头大小不变** ——
   * 所以外层圈照常缩，里面的箭头**反着缩**（`1/t`）：
   *
   *   - 箭头是圈的子元素，它自己的 transform 在圈的局部坐标里；
   *   - 两条动画同一个 duration / easing，且在**同一批**里创建（Svelte 对同一批
   *     过渡按同样的时间点采样、连缓动值都一样），逐帧乘积恰好是 1 ——
   *     屏幕上箭头一直是 18px，不会一屏一帧地抖；
   *   - 圈上有 `overflow-hidden`，所以箭头最后是被**圈裁掉**的（像光圈阖上），
   *     而不是自己缩没；
   *   - **只有箭头淡出**（`opacity: t`），外圈不淡（默认就是 1）：不然收到最后一帧
   *     是箭头中心剩下一个雪亮的白点，比圈本身还显眼（实测就是这个问题）。
   *     外圈全程不透明也刚好让人看清“圈一直在阖”，最后它自己缩没了。
   *     两边都写就是平方（箭头会比圈暗得快），所以只能写一边 —— 写箭头这一边。
   *
   * `1/t` 封顶到 50：t 已经小到圈不足 1px 时，箭头再大也是被裁得什么都不剩，
   * 但没必要让浏览器去摆弄一个几万像素的元素。
   */
  const APERTURE_MS = 200;
  function aperture(node: HTMLElement, { inverse = false }: { inverse?: boolean } = {}) {
    return {
      duration: APERTURE_MS,
      easing: cubicIn,
      css: (t: number) =>
        inverse
          ? `transform: scale(${1 / Math.max(t, 1 / 50)}); opacity: ${t}`
          : `transform: scale(${t})`,
    };
  }

  let lastTop = 0;

  // —— 窗口化 ——
  //
  // 两层窗口，别搞混：
  //
  //   1. **手里有多少** —— `messages`（`App` 管的）。首屏只和服务端要尾巴上那
  //      一窗（`tail`），往上翻到头再去要更早的一窗（`onloadearlier`）。
  //   2. **挂多少进 DOM** —— `limit`（这里管的）。长对话不能把几百条消息全挂
  //      进 DOM（节点数和内存都线性涨），所以只渲染最后 `limit` 条；往上翻先放
  //      手上还没挂的（`limit += step`），手上没有了才去服务端要。
  //
  // 这里是「窗口化」，不只影响布局：DOM 里只挂最后 `step + extra` 条；往上翻
  // 先放手上还没挂的（`extra += step`），手上没有了就找服务端要一窗
  //（`onloadearlier`）。
  //
  // `content-visibility: auto` 只省布局/绘制，**不省 DOM 节点** —— 封顶靠这个。
  //
  // 这里的「条」是**消息**（用户/助手），不是工具调用：一个回合的思考、
  // 工具、正文都是同一条助手消息里的 parts。所以 10 条 = 5 个回合。
  //
  // 没有按钮：全是滚动自己触发的（用户无感）。两个入口：
  //   - `onscroll`：快贴到顶了就去要（提前一屏，滚得快也不会看见顶边露白）
  //   - `fillViewport`：这一窗**比视口还矮**（根本滚不动 —— 短消息很常见）时
  //     自己接着要，否则滚动事件永远不来，那一截历史就再见不到了
  /** 滚到这个位置（px）以内就去要更早的一窗 */
  const LOAD_EARLIER_AT = 400;
  /**
   * 往上翻多放出来的条数（`step` 的整数倍）。
   *
   * 分成 `step + extra` 而不是一个可写的 `limit`：一窗多大是外面说了算的
   *（和发给服务端的 `tail` 同一个数），这里只管「多放了几窗」。
   */
  let extra = $state(0);
  let limit = $derived(step + extra);
  let loadingEarlier = false;

  let start = $derived(Math.max(0, messages.length - limit));

  async function loadEarlier() {
    if (loadingEarlier || !scroller || !content) return;
    loadingEarlier = true;
    // 用户要看更早的了 —— 别再贴底：拉的时候内容一变高，`ResizeObserver` 看见
    // `pinned` 就可能把我们拽回尾部（白拉一窗，视口还跳一下）。

    // 锚定当前视口里**最上面那条消息**，补完把它对回原处。
    //
    // 为什么不用「补进来多少高度就补多少 scrollTop」（以前就是那么干的）：
    // 那个写法默认「这期间容器高度只因为补窗变过」—— 锚点法不依赖任何前提，
    // 只认那一条消息自己的位置（只有**它上面**多出来的东西会移动它）。
    const kids = [...content.children];
    const edge = scroller.getBoundingClientRect().top;
    const anchor = kids.find((el) => el.getBoundingClientRect().bottom > edge);
    const before = anchor?.getBoundingClientRect().top ?? 0;

    // 手上还有没挂出来的就先挂出来；到顶了才去服务端要一窗
    let grew = false;
    try {
      if (start > 0) {
        extra += step;
        grew = true;
      } else if (hasolder && onloadearlier) {
        grew = await onloadearlier();
        // 拿到的那一窗也要真的挂出来（数组变长、窗口不跟着长的话它就压在
        // 窗口上面看不见，用户会以为「往上翻了但是没东西」）
        if (grew) extra += step;
      }
    } finally {
      if (grew) {
        await tick();
        if (anchor) scroller.scrollTop += anchor.getBoundingClientRect().top - before;
        lastTop = scroller.scrollTop;
      }
      loadingEarlier = false;
    }
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
    if (gap < NEAR_BOTTOM) {
      pinned = true;
      // 回到底部了：往上补出来的更早那几窗收回去。窗口只增不减的话，往上翻过一次，
      // 之后整个会话都一直挂在 DOM 里（实测 90 条的长会话：2000 → 5900 个节点）。
      // 在底部收不会跳：被拿掉的都在视口上方，scrollTop 被夹到新的底，贴底照旧。
      //
      // **整屏都装得下的时候不收**：那时「回到底部」是白说的（根本没得滚），
      // 收了只会让 `fillViewport` 又把它要回来 —— 一收一要地反复渲染，
      // 还会一路把历史全拉下来。
      if (extra > 0 && !loadingEarlier && el.scrollHeight > el.clientHeight) extra = 0;
    } else if (el.scrollTop < lastTop - 1) pinned = false;
    lastTop = el.scrollTop;
    // 快到顶了 → 再要一窗更早的
    if (el.scrollTop < LOAD_EARLIER_AT) void loadEarlier();
  }

  /**
   * 手里这一窗**滚不动**（还不够一屏高）却有更早的 → 自己接着要。
   *
   * 没有按钮之后全靠滚动触发，而这种情况恰恰滚不了：滚动事件永远不来，
   * 那一截历史就再见不到了（10 条短消息不到一屏，很常见）。还滚得动就不管，
   * 交给 `onscroll`（它更及时）。
   */
  function fillViewport() {
    if (!scroller || loadingEarlier) return;
    if (!(start > 0 || hasolder)) return;
    if (scroller.scrollHeight > scroller.clientHeight) return;
    void loadEarlier();
  }

  // 手里几条 / 窗口多大 / 还有没有更早 / 视口高度变了 —— 任何一样变了都重看一遍
  //「够不够一屏」。`untrack`：不看 `loadEarlier` 内部读写的那些，不然两边会互相
  // 触发出无限循环（它自己会 `extra += step`、我们会因为 `extra` 变而重跑）。
  $effect(() => {
    void messages.length;
    void limit;
    void hasolder;
    void gap;
    untrack(fillViewport);
  });

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
  // 最后 `step` 条（见下面的窗口化），封顶靠它，不靠 content-visibility。

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
    // 切会话 → 挂出来的窗口收回来（否则新会话只显示尾巴那几条）。
    // **不能盯 `messages[0]`**：往上翻一窗也会换掉它（服务端那一窗插到前面），
    // 那时候把窗口收回去等于刚翻出来的又收起来了。
    extra = 0;
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
              <Img src={a.url} alt={a.name} class="block max-h-48 max-w-full object-cover" />
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
     底色和边框则跟**输入卡片本身**一致（`bg-surface/92` + `border-control`）：
     它是浮在正文上的，底下有字经过时透出一点点才看得出是个浮层；
     但透太多就变成“糊了一层”的脏块（试过 /85：正文直接读过圆钮，像没盖住），
     92% 是刚刚好那档。
     （卡片里那两个圆按钮是 `bg-control`、没有边 —— 照抄它们就只能是个实心疙瘩。）
     只有图标没有文字（`aria-label` / `title` 顶上），就跟停止键一样。

     出现 / 消失走 `aperture`（见上面那段）：圈从**一个点**长出来、收回去也小到那个点
     再没，而里头的箭头**一直 18px** —— 圈阖到比箭头还小时把它裁掉。
     **外圈全程不透明，只有箭头淡出**：否则最后一帧是箭头中心一个雪亮的白点，
     比圈还显眼（实测）。
     200ms / `cubicIn`（先慢后快：前 1/5 时间只走 0.8%，最后 1/5 走 48.8%），
     圈先摆在那儿几乎不动，最后一下阖进那个点。
     缩放中心是元素自己的中心（`transform-origin` 默认 50% 50%），左右两边等速往中间收。
     圈要 `overflow-hidden` 才能把箭头裁掉；而它和箭头是两层，各管各的缩放。

     `absolute` 挂在 App 的 `main`（`absolute inset-0`）上，所以宽屏有侧边栏
     时它跟着**对话区**居中，不是整个窗口居中。底部避让用跟滚动容器 padding
     同一个值（`--composer-h`）—— 输入卡片的高度一变，两边一起动。 -->
{#if showJump}
  <button
    type="button"
    onclick={jumpToLatest}
    aria-label="回到最新"
    title="回到最新"
    class="absolute bottom-[calc(var(--composer-h,0px)+1.25rem)] left-1/2 z-10 size-9 -translate-x-1/2
           rounded-full text-fg"
  >
    <!-- 圈：底色 + 跟输入卡片同一条边。它是缩的那一层，也负责裁里面的箭头 -->
    <span
      transition:aperture
      class="grid size-9 place-items-center overflow-hidden rounded-full border border-control
             bg-surface/92"
    >
      <!-- 箭头：反着缩，屏幕上尺寸不变。包一层是为了让它居中、并且不参与圈那条
           transition；这层**就是 18px**（跟图标一样大）—— 36px 的话它的中心和
           圈的中心会差半个「边框 + grid 居中」的量（实测中心抖 1px）。 -->
      <span transition:aperture={{ inverse: true }} class="block size-[18px]">
        <Icon name="down" size={18} />
      </span>
    </span>
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
          <!-- 结果：模型收到的原文，原样（退出码、截断、续读位置都在正文里） -->
          <div class="flex flex-col gap-2">
            <span class="text-meta {res.failed ? 'text-bad' : 'text-faint'}">
              {t.state === 'output' ? '模型收到的结果' : '运行中…'}
            </span>

            {#each res.images as img}
              <!-- 工具返回的图：展示原图（沙箱路由），模型看的是缩放后的那份 -->
              <Img
                src={sandboxPath(img)}
                alt="工具返回的图片"
                class="max-h-80 w-auto self-start rounded-xl bg-hairline-soft object-contain"
              />
            {/each}

            {#if res.diff}
              <div
                class="overflow-x-auto rounded-xl bg-hairline-soft px-3 py-2.5 font-mono text-[13px] leading-5"
              >
                {#each res.text.split('\n') as line}
                  <p class="whitespace-pre {diffLine(line)}">{line || ' '}</p>
                {/each}
              </div>
            {:else}
              <pre
                use:followTail
                class="font-mono text-[13px] leading-5 whitespace-pre-wrap break-all {res.failed
                  ? 'text-bad'
                  : ''}">{res.text}</pre>
            {/if}
          </div>
        {:else if t.state === 'streaming'}
          <p class="text-sm text-muted">执行中…</p>
        {/if}
      </div>
    </Sheet>
  {/if}
{/if}
