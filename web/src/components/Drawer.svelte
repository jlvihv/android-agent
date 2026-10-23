<script lang="ts">
  /**
   * 抽屉（侧边栏）。
   *
   *   [ 搜索对话… ]                  ← 顶上就一个搜索框，输入即搜（跨会话搜正文）
   *   💬 新对话                     ← 没搜 = 最近对话；搜了 = 命中（标题 + 摘要）
   *   💬 帮我看看屏幕上是什么界面
   *   (⚙)                     jl 🖥  ← 齿轮（左）+ 现在连的是哪台机器（右对齐）
   *
   * 去掉了：agent 标题栏 + 版本号、事件入口、分割线、「最近对话」标题。
   * 搜索框替掉标题栏：边栏本来就是用来找对话的，搜索比版本号有用。
   * 底栏（`px-4` 那一行）：左边齿轮 = 设置，最右边是**现在连的是哪台机器**
   * （手机自己的核心 =「本机」，连着电脑 = 那台电脑的名字，见 `machine` prop）。
   *
   * 底栏那个白底的「新建会话」也去掉了 —— 它是这一屏最亮的东西，比对话本身还
   * 抢眼；开新对话本来就有头部右上角那个图标，不需要再来一个。
   *
   * **长按**对话行 → 重命名 / 删除。跟 Claude 一样，列表里不放常驻按钮。
   *
   * 两种形态（`pinned`）：
   *   窄屏 —— 抽屉：盖在对话上（遮罩 / 往左拖 / 返回键 / 滑进滑出）
   *   宽屏 —— 并排的一栏：没有遮罩、不能拖、不接返回键，头部那个菜单按钮也没了
   * 中间那一段（搜索框 + 列表 + 底栏）是同一个 snippet，两边都用它。
   *
   * 列表里**收藏的页面**在会话上面（不搜索时）：AI 做的网页不用回聊天记录里找，
   * 点一下直接在查看器里打开。长按收藏行 = 取消收藏；收藏本身在查看器右上角的星。
   *
   * DOM 刻意浅：aside 只有三个孩子（搜索框 / 列表 / 底栏），
   * 中间那层仅仅是滚动容器 —— 滚动区必须是单个元素，省不掉。
   *
   * 动画：进出场是 Svelte 过渡（CSS 动画，跑在合成线程上）；
   * 往左拖可以跟手关掉。遮罩也在这里，跟面板一起淡入淡出。
   * 没有关闭按钮：往左拖、点遮罩、按返回键都能关。
   */
  import { fade, slide } from "svelte/transition";
  import { quintOut } from "svelte/easing";
  import { onBack } from "../lib/overlay";
  import { dismissable, RELEASE } from "../lib/drag";
  import Icon from "./Icon.svelte";
  import { untrack } from "svelte";
  import { machines } from "../lib/machines.svelte.ts";
  import { favorites, type Favorite } from "../lib/favorites.svelte.ts";
  import { sandboxPath } from "../lib/markdown";
  import { openViewer } from "../lib/viewer.svelte.ts";

  export type Session = {
    id: string;
    title: string;
    active: boolean;
    count: number;
    /** 这个会话有回合在跑（可以同时在几个会话里各跑一个） */
    busy?: boolean;
  };
  /** 搜索命中（GET /api/search） */
  export type Hit = {
    id: string;
    title: string;
    active: boolean;
    role: string;
    snippet: string;
  };

  let {
    sessions,
    onclose,
    onswitch,
    ondelete,
    onrename,
    onsettings,
    /** 宽屏：**并排**的一栏（不是抽屉）。见 lib/wide.svelte.ts */
    pinned = false,
    /**
     * 从对话区往右拖着打开时，手指拉出来的比例（0 = 还完全在外面，1 = 整个拉出来）。
     * `null` = 不在跟手（点菜单打开、或者已经松手）。见 App.svelte 的 `openDrag`。
     */
    peek = null,
    /** 现在连的是哪台机器（`store.machine`）：`phone` = 手机自己的核心 */
    machine,
    onmachine,
  }: {
    sessions: Session[];
    onclose: () => void;
    onswitch: (id: string) => void;
    ondelete: (id: string) => void;
    onrename: (id: string, title: string) => void;
    /** 左下角的齿轮 → 设置页 */
    onsettings: () => void;
    pinned?: boolean;
    peek?: number | null;
    machine: { name: string; phone: boolean };
    /** 点底栏那个机器标签 → 「选择机器」面板。**壳不在时是 null**
     *  （电脑浏览器 / 手机浏览器里只有一个核心，没得换）——那就只显示，不可点。 */
    onmachine: (() => void) | null;
  } = $props();

  /**
   * 侧栏一出来就把「选择机器」的数据拉好：面板是从底栏那个标签点开的，等点开再拉的话，
   * 第一帧只有「本机」一行，其余几行随后跳出来（`machines` 的数据活在组件外面，
   * 面板挂上时直接用这份）。点不了的时候（没有 `onmachine`）不拉。
   */
  $effect(() => {
    if (onmachine) void untrack(() => machines.load());
  });

  // 收藏列表也是「一拉就有」：模块级那份（查看器里点星会直接改它），
  // 这里只负责确保它已经拉过。
  $effect(() => {
    void favorites.load();
  });

  /** 搜索防抖（ms）：输入停顿这么久再打后端。一边打字一边发请求纯属浪费；
   *  effect 的清理函数会把上一个计时器干掉，所以快速输入不会堆积请求。 */
  const SEARCH_DEBOUNCE_MS = 180;
  /** 长按多久算长按（ms）。太短会和滚动/点击打架，太长会让人觉得没反应 */
  const LONG_PRESS_MS = 420;
  /** 长按触发的轻震（ms） */
  const HAPTIC_MS = 12;
  /** 宽屏下侧边栏收 / 展的宽度动画时长（ms） */
  const PIN_MS = 180;
  /** 抽屉滑进 / 滑出的时长（ms）。出场是「先快后慢」：从当前位置继续往左收，
   *  剩下的距离越短越快收完，所以是 `SLIDE_OUT_MS * 剩余比例 + SLIDE_OUT_MIN_MS`
   *  —— 拖到一半松手时手指本来就在动，减速收尾才接得住。 */
  const SLIDE_IN_MS = 320;
  const SLIDE_OUT_MS = 240;
  const SLIDE_OUT_MIN_MS = 80;
  /** 遮罩淡入 / 淡出的时长（ms） */
  const SCRIM_FADE_MS = 220;
  /** 长按菜单的尺寸（rem）：`w-40` = 10 宽；两行 `py-3.5` + 留白 ≈ 8 高，
   *  一行（收藏那个「取消收藏」）≈ 4 高。弹出时要把它夹在抽屉（`--drawer-w`）
   *  和屏幕里，所以这几个数得跟着菜单改。 */
  const MENU_W = 10;
  const MENU_H = 8;
  const MENU_ONE_H = 4;
  /** 菜单离抽屉 / 屏幕边留的空隙（rem） */
  const MENU_GAP = 0.5;

  // —— 搜索 ——
  let q = $state("");
  let hits = $state<Hit[]>([]);
  let searching = $state(false);

  /**
   * 命中按会话分组。
   *
   * 平铺的话，同一个会话的三条命中会把标题重复三遍，而且
   * `h.active`（当前打开的那个会话）会给其中几条涂上 #414141 ——
   * 看着像“这几条被选中了”，其实跟搜索无关。
   */
  const groups = $derived.by(() => {
    const m = new Map<string, { id: string; title: string; hits: Hit[] }>();
    for (const h of hits) {
      let g = m.get(h.id);
      if (!g) m.set(h.id, (g = { id: h.id, title: h.title, hits: [] }));
      g.hits.push(h);
    }
    return [...m.values()];
  });

  /** 摘要里全是 markdown 记号，直接显示很脏（`**1. \`bash\`**`）。去记号留字。
      只去强调和反引号，以及最前面那个列表项标记 —— 正文里的 `1)` `-` 是内容。 */
  function plain(s: string): string {
    return s
      .replace(/\*\*|__|`{1,3}/g, "")
      .replace(/^\s*(#+|\d+[.)]|[-*+])\s+/, "")
      .replace(/\s+/g, " ")
      .trim();
  }

  /** 把摘要按命中词切成段，命中那段提亮 —— 不然看不出为什么它被搜出来 */
  function mark(text: string, needle: string): { t: string; hit: boolean }[] {
    const n = needle.trim();
    if (!n) return [{ t: text, hit: false }];
    const out: { t: string; hit: boolean }[] = [];
    const lower = text.toLowerCase();
    const nl = n.toLowerCase();
    let i = 0;
    for (;;) {
      const at = lower.indexOf(nl, i);
      if (at < 0) break;
      if (at > i) out.push({ t: text.slice(i, at), hit: false });
      out.push({ t: text.slice(at, at + n.length), hit: true });
      i = at + n.length;
    }
    if (i < text.length) out.push({ t: text.slice(i), hit: false });
    return out;
  }

  $effect(() => {
    const needle = q.trim();
    if (!needle) {
      hits = [];
      searching = false;
      return;
    }
    searching = true;
    const t = setTimeout(() => void search(needle), SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(t);
  });

  async function search(needle: string) {
    try {
      const r = await fetch(`/api/search?q=${encodeURIComponent(needle)}`);
      if (r.ok) hits = ((await r.json()) as { hits: Hit[] }).hits ?? [];
    } catch {
      /* 后端不在就当没搜到 */
    } finally {
      searching = false;
    }
  }

  // —— 拖拽关闭 ——
  //
  // 按住抽屉任何地方往左拖都跟手（包括会话列表上）；竖着划还是滚列表。
  // 手势判定见 lib/drag.ts。
  let aside: HTMLElement | null = $state(null);
  /**
   * 被往左拖出去的比例，0 = 完全展开，1 = 完全收起。
   *
   * 跟着手指拉出来的那种，第一帧就从手指的位置算（下面那个 `$effect` 要等挂上之后才跑）——
   * 不然挂上那一刻是 0（整个展开、遮罩全黑），会闪一下。
   */
  // svelte-ignore state_referenced_locally
  let off = $state(peek != null ? 1 - peek : 0);
  /** 跟手中：关掉回弹过渡，否则拖起来发黏 */
  let dragging = $state(false);

  const drag = {
    axis: "x" as const,
    dir: -1 as const,
    size: () => aside?.offsetWidth ?? 1,
    onStart: () => {
      dragging = true;
      clearTimeout(pressTimer);
    },
    onMove: (v: number) => (off = v),
    onEnd: (close: boolean) => {
      dragging = false;
      if (close) onclose();
      else off = 0;
    },
  };

  // 从对话区往右拖着打开：位置由外面的手指定（`peek`），这期间关掉回弹过渡。
  // 松手展开时 `peek` 变回 null：回到 0，由 RELEASE 过渡接着走完；松手收回则是外面
  // 直接卸掉抽屉，出场动画从当前位置接着往左收（见 slideOut）。
  let peeking = false;
  $effect(() => {
    if (peek != null) {
      peeking = true;
      dragging = true;
      off = 1 - peek;
    } else if (peeking) {
      peeking = false;
      dragging = false;
      off = 0;
    }
  });

  // 返回键关抽屉；拖拽三件套也是「预测性返回」的跟手位移
  // （系统手势划到哪，抽屉就跟着挪到哪。Android 14+；桌面可手动 __predictiveBack 模拟）
  //
  // 并排那一栏**不接**返回键：它不是盖在东西上面的，返回键没有「关掉它」这回事
  // （在桌面浏览器上按返回应该是浏览器自己的历史）。
  $effect(() => {
    if (pinned) return;
    return onBack(() => onclose(), drag);
  });

  /** 进场：从左边整个滑进来（跟着手指拉出来的那种不要进场动画：位置归手指管） */
  function slideIn(_: Element) {
    if (peek != null) return { duration: 0 };
    return {
      duration: SLIDE_IN_MS,
      easing: quintOut,
      css: (_t: number, u: number) => `transform: translateX(${-u * 100}%)`,
    };
  }

  /**
   * 遮罩进场：淡入。跟着手指拉出来的那种**不要**：遮罩的深浅归手指管（`1 - off`）。
   *
   * 不能直接 `transition:fade`：它把挂上那一刻的透明度当终点，用 CSS 动画做 0→1 ——
   * CSS 动画盖过内联样式，那 220ms 里遮罩不管手指在哪都自己变黑，动画一完又回到手指
   * 对应的那一点点，看着就是「闪一下」。
   */
  function scrimIn(node: Element) {
    if (peek != null) return { duration: 0 };
    return fade(node, { duration: SCRIM_FADE_MS });
  }

  /** 出场：从当前位置（可能被拖到一半）继续往左收（时间怎么算见上面常量） */
  function slideOut(_: Element) {
    const from = off;
    return {
      duration: SLIDE_OUT_MS * (1 - from) + SLIDE_OUT_MIN_MS,
      easing: quintOut,
      css: (_t: number, u: number) =>
        `transform: translateX(${-(from + (1 - from) * u) * 100}%)`,
    };
  }

  // 长按弹出的菜单。会话两行（重命名 / 删除），收藏一行（取消收藏）
  let menu = $state<{
    kind: "session" | "page";
    id: string;
    x: number;
    y: number;
  } | null>(null);
  // 正在重命名的行
  let renaming = $state<string | null>(null);
  let draft = $state("");

  let pressTimer: ReturnType<typeof setTimeout> | undefined;
  let longPressed = false;

  function pressStart(e: PointerEvent, kind: "session" | "page", id: string) {
    longPressed = false;
    clearTimeout(pressTimer);
    const { clientX: x, clientY: y } = e;
    pressTimer = setTimeout(() => {
      longPressed = true;
      menu = { kind, id, x, y };
      navigator.vibrate?.(HAPTIC_MS);
    }, LONG_PRESS_MS);
  }
  const pressEnd = () => clearTimeout(pressTimer);

  /** 点收藏行：直接打开那个页面，顺手收起抽屉（宽屏那一栏不受影响） */
  function openPage(f: Favorite) {
    openViewer(sandboxPath(f.path), f.title);
    onclose();
  }

  function pick(id: string) {
    const s = sessions.find((v) => v.id === id);
    draft = s?.title ?? "";
    renaming = id;
    menu = null;
  }

  /** autofocus 会触发 a11y 警告，用 action 自己聚焦 */
  function focusOnMount(node: HTMLInputElement) {
    queueMicrotask(() => {
      node.focus();
      node.select();
    });
  }

  function commitRename() {
    if (renaming) onrename(renaming, draft.trim() || "新对话");
    renaming = null;
  }
</script>

{#snippet body()}
  <!-- 不浮动：搜索框就是顶上占一行的一块。
       全圆角 + 里面一个放大镜。放大镜是 CSS 背景图（不套 div/span）：
       纯装饰，为它多一层 DOM 不值当。图形跟 Icon.svelte 一个规格
       （20 画布 / 描边 1.8），颜色取 muted。 -->
  <input
    bind:value={q}
    type="search"
    placeholder="搜索对话…"
    spellcheck="false"
    autocomplete="off"
    aria-label="搜索对话"
    class="search-input mx-4 mt-1 mb-3 shrink-0 rounded-full bg-surface py-3 pr-4 pl-10 text-base
           text-fg placeholder:text-faint outline-none"
  />

  <!-- 列表占中间：上下各留 --fade-top 的白，两块渐隐正好盖在这两条白上
       （`.drawer-shell::before/::after`，定位见 app.css）—— 内容滑出去时
       在硬边上淡掉，而不是被切一刀。留白不能比渐隐短：不然第一条会话
       一进来就停在渐隐里，是半透明的。
       左右边距跟搜索框和底栏对齐（`px-4`）—— 都是顶层表面，内部要一个值。
       行自己的 `px-3` 是药丸内的内边距，不是这里说的边距。 -->
  <div
    class="min-h-0 flex-1 overflow-y-auto px-4 pt-[var(--fade-top)] pb-[var(--fade-top)]"
  >
    <!-- 收藏的页面：AI 做的网页收藏后列在这儿，点一下直接开（不搜索时才有；
         搜索是在找对话，这时候把页面混进结果里没有意义）。 -->
    {#if !q.trim() && favorites.list.length}
      <p class="flex items-center gap-2 px-3 pt-3 pb-1 text-xs text-faint">
        <Icon name="star" size={13} class="shrink-0 text-muted" />
        <span class="truncate">收藏的页面</span>
      </p>
      {#each favorites.list as f (f.path)}
        <button
          onpointerdown={(e) => pressStart(e, "page", f.path)}
          onpointerup={pressEnd}
          onpointercancel={pressEnd}
          oncontextmenu={(e) => e.preventDefault()}
          onclick={() => {
            if (longPressed) return;
            openPage(f);
          }}
          class="flex w-full items-center gap-3 rounded-2xl px-3 py-3 text-left"
        >
          <Icon name="file" size={18} class="shrink-0 text-muted" />
          <span class="min-w-0 flex-1">
            <span class="block truncate text-base">{f.title}</span>
            <span class="block truncate text-meta text-faint">{f.path}</span>
          </span>
        </button>
      {/each}
    {/if}

    {#if q.trim()}
      {#if hits.length === 0}
        <p class="px-3 py-3 text-sm text-faint">
          {searching ? "搜索中…" : "没有匹配的对话"}
        </p>
      {:else}
        {#each groups as g (g.id)}
          <!-- 会话名只在组头出现一次（图标跟会话列表一致） -->
          <p class="flex items-center gap-2 px-3 pt-3 pb-1 text-xs text-faint">
            <Icon name="chat" size={13} class="shrink-0 text-muted" />
            <span class="truncate">{g.title}</span>
          </p>
          {#each g.hits as h, i (`${g.id}:${i}`)}
            <button
              onclick={() => onswitch(g.id)}
              class="block w-full rounded-2xl px-3 py-2 text-left"
            >
              <span class="line-clamp-2 text-sm"
                ><span class="mr-1.5 text-meta text-faint"
                  >{h.role === "user" ? "你" : "agent"}</span
                >{#each mark(plain(h.snippet), q) as p}{#if p.hit}<mark
                      class="bg-transparent font-medium text-fg">{p.t}</mark
                    >{:else}<span class="text-muted">{p.t}</span
                    >{/if}{/each}</span
              >
            </button>
          {/each}
        {/each}
      {/if}
    {:else if sessions.length === 0}
      <p class="px-3 py-3 text-sm text-faint">还没有对话</p>
    {:else}
      {#each sessions as s (s.id)}
        {#if renaming === s.id}
          <input
            bind:value={draft}
            use:focusOnMount
            onblur={commitRename}
            onkeydown={(e) => e.key === "Enter" && commitRename()}
            class="my-1 w-full rounded-2xl bg-active px-3 py-3 text-base text-fg outline-none"
          />
        {:else}
          <button
            onpointerdown={(e) => pressStart(e, "session", s.id)}
            onpointerup={pressEnd}
            onpointercancel={pressEnd}
            oncontextmenu={(e) => e.preventDefault()}
            onclick={() => {
              if (longPressed) return;
              onswitch(s.id);
            }}
            class="flex w-full items-center gap-3 rounded-2xl px-3 py-3 text-left
                   {s.active ? 'bg-active' : ''}"
          >
            <Icon name="chat" size={18} class="shrink-0 text-muted" />
            <span class="min-w-0 flex-1 truncate text-base">{s.title}</span>
            {#if s.busy}
              <!-- 还在跑：切走了也看得出来 -->
              <span
                class="size-1.5 shrink-0 animate-pulse rounded-full bg-warn"
                aria-label="运行中"
              ></span>
            {/if}
          </button>
        {/if}
      {/each}
    {/if}
  </div>

  <!-- 底栏也是流里的一块（不浮动）。 -->
  <div
    class="flex shrink-0 items-center gap-3 px-4 pt-3 pb-[max(0.75rem,env(safe-area-inset-bottom))]"
  >
    <!-- 左下角 = 设置。图标本身说明用途。 -->
    <button
      type="button"
      onclick={onsettings}
      aria-label="设置"
      title="设置"
      class="grid size-11 shrink-0 place-items-center rounded-full bg-hairline text-muted"
    >
      <Icon name="gear" size={20} />
    </button>

    <!-- 最右边 = 现在连的是哪台机器。整个侧栏就这一处能回答「我在跟谁说话」——
         界面、命令、文件到底在手机上还是在电脑上，不看它得点进设置才知道。
         手机自己的核心写「本机」；连着电脑就写那台电脑的名字（`/api/health`
         的 `computer`，和手机搜到的、配对码那一栏是同一个名字）。
         图标在名字**右边**（贴右边缘），像行尾那个 `›` ：它点开的是「选择机器」
         面板，就地换一台（面板里的内容见 MachineSheet.svelte）。没有壳的时候
         （电脑浏览器 / 手机浏览器）只有一个核心，没得换 —— 那它就不是按钮。
         `text-sm` + `muted`，和输入框那个模型药丸同档。名字太长就截断：`min-w-0`
         让整块可以在需要时收窄（不写死宽度上限）。 -->
    <svelte:element
      this={onmachine ? "button" : "span"}
      type={onmachine ? "button" : undefined}
      role={onmachine ? undefined : "status"}
      onclick={onmachine ?? undefined}
      title={onmachine
        ? `现在连的是${machine.name}（点一下换一台）`
        : `现在连的是${machine.name}`}
      class="ml-auto flex min-w-0 items-center gap-1.5 text-sm text-muted"
    >
      <span class="truncate">{machine.name}</span>
      <Icon
        name={machine.phone ? "phone" : "computer"}
        size={15}
        class="shrink-0"
      />
    </svelte:element>
  </div>
{/snippet}

{#if pinned}
  <!-- 宽屏：并排的一栏。没有遮罩、不能往左拖、也不接返回键 ——
       它本来就不是「盖在上面」的东西，宽度写死。
       收 / 展是头部那个菜单按钮（`in`/`out` 这里负责动画：宽度收到 0）。
       **`relative` 不能少**：上下两条渐隐（`.drawer-shell::before/::after`）是
       absolute + inset-inline:0，没有定位祖先就按整个视口铺 —— 会横穿对话区，
       在滚动条和正文上压出两道暗带（踩过）。窄屏那个是 fixed，不受影响。 -->
  <aside
    in:slide|global={{ axis: "x", duration: PIN_MS }}
    out:slide|global={{ axis: "x", duration: PIN_MS }}
    class="drawer-shell relative flex h-full w-72 shrink-0 flex-col border-r border-hairline bg-drawer
           pt-[env(safe-area-inset-top)]"
  >
    {@render body()}
  </aside>
{:else}
  <button
    class="fixed inset-0 z-40 bg-scrim"
    style:opacity={1 - off}
    style:transition={dragging ? "none" : "opacity 280ms ease-out"}
    aria-label="关闭"
    onclick={onclose}
    in:scrimIn|global
    out:fade|global={{ duration: SCRIM_FADE_MS }}
  ></button>

  <aside
    bind:this={aside}
    use:dismissable={drag}
    style:transform={off ? `translateX(${-off * 100}%)` : null}
    style:transition={dragging ? "none" : RELEASE}
    class="drawer-shell fixed top-0 bottom-[var(--kb,0px)] left-0 z-50 flex w-[var(--drawer-w)]
           flex-col bg-drawer pt-[env(safe-area-inset-top)] will-change-transform"
    in:slideIn|global
    out:slideOut|global
  >
    {@render body()}
  </aside>
{/if}

{#if menu}
  {@const m = menu}
  <!-- 长按菜单。点别处关掉。 -->
  <button
    class="fixed inset-0 z-[60]"
    aria-label="关闭菜单"
    onclick={() => (menu = null)}
  ></button>
  <div
    class="fixed z-[61] w-40 overflow-hidden rounded-2xl bg-bubble shadow-2xl shadow-drop"
    style="left: min({m.x}px, calc(var(--drawer-w) - {MENU_W + MENU_GAP}rem));
           top: min({m.y}px, calc(100dvh - {(m.kind === 'page'
      ? MENU_ONE_H
      : MENU_H) + MENU_GAP}rem))"
  >
    {#if m.kind === "page"}
      <!-- 收藏行只有一件事可做：取消（重命名没做 —— 标题是页面自己的，
           真嫌长了改页面 <title> 更直接） -->
      <button
        onclick={() => {
          void favorites.remove(m.id);
          menu = null;
        }}
        class="flex w-full items-center gap-3 px-4 py-3.5 text-left text-base text-fg"
      >
        <Icon name="star" size={18} filled class="text-muted" />
        取消收藏
      </button>
    {:else}
      <button
        onclick={() => pick(m.id)}
        class="flex w-full items-center gap-3 px-4 py-3.5 text-left text-base text-fg"
      >
        <Icon name="pencil" size={18} class="text-muted" />
        重命名
      </button>
      <button
        onclick={() => {
          ondelete(m.id);
          menu = null;
        }}
        class="flex w-full items-center gap-3 px-4 py-3.5 text-left text-base text-danger"
      >
        <Icon name="trash" size={18} />
        删除
      </button>
    {/if}
  </div>
{/if}
