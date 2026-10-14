<script lang="ts">
  /**
   * 选择模型 / 思考强度。从输入卡片上的模型药丸打开。
   *
   * 两页共用一个面板，横向平移切换 —— 面板本身不重新弹出、不重新收起，
   * 只有内容在滑，所以看着「丝滑」。动效全是原生 CSS：
   *   - 位移：`transform: translateX(...)` + `transition: transform`
   *   - 高度：`height` 也跟着当前页变（面板会一起长高/变矮），同样是 CSS 过渡
   * JS 只做两件事：记「现在哪一页」、量一下两页的自然高度（`height: auto → auto`
   * 没法过渡，只能给它一个具体数值）。
   *
   * 布局照着 Claude 的「选择模型」面板，几处不同：
   *   1. **不写模型描述**。Claude 每行下面有一句「适用于复杂工作与日常任务」；
   *      那是给它几个自家模型手写的，我们这里是 provider `/models` 拉回来的
   *      任意清单（OpenCode Go 有 38 个），没有描述可写，也不该编。
   *   2. **模型清单单独滚动**：38 行会把「思考强度」推到屏幕外，得先划过 38 行
   *      才点得到。给清单一个高度上限（内容在里面滚），下面两块就永远在视野里。
   *   3. 清单上面多一行「**模型提供商**」：点进去（和思考强度一样横移到第二页）
   *      只列配好了 Key 的几家，选一家，清单就换成那家的模型。点了模型才真的
   *      切过去 —— Key 在设置页里各存一份，不用重填。那一页最后一行「管理模型
   *      提供商」打开设置页（添加 / 删除 Key）。
   *
   * 清单本身是**全局缓存**的，不在这个组件里（见 `lib/models.svelte.ts`）：
   * 面板每开一次都是一个新组件（`App.svelte` 里的 `{#if sheet}`），清单要是存在
   * 组件里，打开时第一帧就只有「载入中…」一行，十几毫秒后 38 行到了面板再长高
   * —— 那一下就是布局跳动。缓存放在组件外面，第一帧就有数据，高度一次到位。
   *
   * 第二页没有返回箭头：安卓本来就有返回键/返回手势，再画一个纯属重复。
   * 返回键在 `Sheet` 里被接管（见它的 `onback`），按下是退回第一页。
   */
  import { untrack } from 'svelte';
  import Icon from './Icon.svelte';
  import Sheet from './Sheet.svelte';
  import { store } from '../lib/store.svelte.ts';
  import { models, type Group, type Model } from '../lib/models.svelte.ts';
  import { EFFORTS, effortLabel } from '../lib/effort';
  import { PAGE_MS } from '../lib/drag';
  import type { Peek } from '../lib/overlay';

  let {
    onclose,
    onconfig,
    rev = 0,
  }: {
    onclose: () => void;
    /** 「管理模型提供商」→ 打开设置页（各家的 API Key） */
    onconfig: () => void;
    /** 设置页关掉时会变：Key 可能改了，清单要重拉 */
    rev?: number;
  } = $props();

  type Page = 'models' | 'sub';
  /** 第二页放什么。两个子页共用轨道上的第二个位置（不排成三页：从第一页到
   *  第三页会把中间那页也扫一遍）。返回时不清，让它滑出去时内容还在。 */
  type Sub = 'effort' | 'providers';

  let page = $state<Page>('models');
  let sub = $state<Sub>('effort');
  /** 清单正在看哪家（模型提供商页选的）。默认是当前在用的那家。 */
  let viewing = $state<string | null>(null);
  const shown = $derived(models.groups.find((g) => g.id === viewing) ?? models.groups[0] ?? null);
  /** 当前在用的是哪家（服务商 id） */
  const currentProvider = $derived(models.provider);
  const modelCount = $derived(models.groups.reduce((n, g) => n + g.models.length, 0));
  let busy = $state(false);
  let scroller = $state<HTMLDivElement | null>(null);
  let paneModels = $state<HTMLDivElement | null>(null);
  let paneSub = $state<HTMLDivElement | null>(null);
  let hModels = $state(0);
  let hSub = $state(0);

  const current = $derived(store.llm?.model ?? '');
  const effort = $derived(store.llm?.reasoningEffort ?? null);
  /** 预测性返回的进度（只在第二页）：0 = 原位，1 = 已完全回到第一页 */
  let peekP = $state(0);
  /** 跟手中：关掉过渡，不然手指和内容之间有延迟 */
  let peeking = $state(false);

  /** 0 = 在第一页，1 = 在第二页；跟手返回时介于两者之间 */
  const at = $derived(page === 'sub' ? 1 - peekP : 0);
  /** 视口高度跟着当前页走：切页时整个面板一起长高/变矮；跟手时按进度插值 */
  const height = $derived(hModels + (hSub - hModels) * at);
  const ease = 'cubic-bezier(0.32,0.72,0,1)';

  const back = () => (page = 'models');
  const openSub = (s: Sub) => {
    sub = s;
    page = 'sub';
  };

  /** 在模型提供商页选了一家：回到清单，换成这家的模型（滚动交给下面的 effect） */
  function view(id: string) {
    viewing = id;
    back();
  }

  /**
   * 第二页的跟手返回：第一页从左边跟着手指滑回来（和点进来时的方向相反），
   * 面板本身不动。松手完成就接着滑完，撤回就弹回第二页。
   */
  const peek: Peek = {
    onStart: () => {
      peeking = true;
      peekP = 0;
    },
    onMove: (v) => (peekP = v),
    onEnd: (commit) => {
      peeking = false;
      if (commit) back();
      peekP = 0;
    },
  };

  // 量两页的自然高度。两个 pane 都在 DOM 里（在轨道上），量的是它们各自
  // 不受视口限制时的高度。清单到位后（`loaded` / `modelCount`）要重量。
  $effect(() => {
    void modelCount;
    void models.loaded;
    void viewing;
    void sub;
    if (paneModels) hModels = paneModels.offsetHeight;
    if (paneSub) hSub = paneSub.offsetHeight;
  });

  // 清单交给全局缓存：新鲜的话**一个请求都不发**（打开面板就是瞬间的，没有
  // 「载入中…」那一帧）；过期就先拿旧的渲染、背后悄悄重拉。
  //
  // 用 `untrack` 包住：`load()` 内部会同步读 `loaded` / `err`，不挡住的话这就是
  // 一个依赖，拉失败时「write err → effect 重跑 → 再拉」能死循环。这块只该
  // 跟着 `rev` 走。（渲染不受影响：模板直接读 `models.groups`，自己会更新。）
  $effect(() => {
    void rev;
    void untrack(() => models.load(rev));
  });

  // 正在看的那家还在就不动（设置页里改完 Key 回来别跳走），否则看当前这家。
  // 写的是自己读的 `viewing` —— 对得上就不写，不会来回触发。
  $effect(() => {
    const gs = models.groups;
    if (gs.length === 0) return;
    if (!gs.some((g) => g.id === viewing)) viewing = models.provider ?? gs[0].id;
  });

  /**
   * 把「当前在用的那个模型」滚到视野中间。
   *
   * 38 个模型，当前那个多半在折叠线以下 —— 不滚过去的话，打开面板只看得到
   * 一屏陌生名字，还得自己找。**挂载时就要滚**：缓存命中时数据第一帧就在了，
   * 等请求回来再滚的话，面板早画好了才动，又是一跳。
   */
  function scrollToCurrent() {
    const s = scroller;
    if (!s) return;
    const cur = s.querySelector('[data-current="1"]');
    // 找不到（当前模型不属于这家）：回到顶部，别停在上一家的位置
    if (cur) cur.scrollIntoView({ block: 'center' });
    else s.scrollTop = 0;
  }

  // 清单到位、或者换了看哪家之后，下一帧再滚（行得先渲染出来）
  $effect(() => {
    void modelCount;
    void viewing;
    if (modelCount) requestAnimationFrame(scrollToCurrent);
  });

  const isCurrent = (g: Group, m: Model) => g.id === currentProvider && m.id === current;

  /** 换模型（可能换到另一家）：强度字段跟着一起发（空串 = 保持默认档）。 */
  async function pickModel(g: Group, m: Model) {
    if (busy || isCurrent(g, m)) return;
    await apply(m.id, g.id, effort);
  }

  /** 换强度：模型和服务商照原样带上。 */
  async function pickEffort(value: string | null) {
    if (busy || value === effort) return;
    await apply(current, currentProvider, value);
  }

  async function apply(model: string, provider: string | null, e: string | null) {
    busy = true;
    models.err = '';
    try {
      const r = await fetch('/api/model', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ model, provider: provider ?? undefined, reasoningEffort: e ?? '' }),
      });
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      models.setProvider(provider);
      await store.reloadHealth();
    } catch (e) {
      models.err = String(e);
    } finally {
      busy = false;
    }
  }
</script>

<Sheet
  title={page === 'sub' ? (sub === 'effort' ? '思考强度' : '模型提供商') : '选择模型'}
  onback={page === 'sub' ? back : undefined}
  {peek}
  slide={page === 'sub' ? 1 : -1}
  hug
  padClass="px-0"
  {onclose}
>
  <!-- 视口：横向把另一页裁掉，高度跟着当前页（CSS 过渡） -->
  <div
    class="overflow-hidden"
    style:height={hModels ? `${height}px` : undefined}
    style:transition={peeking ? 'none' : `height ${PAGE_MS}ms ${ease}`}
  >
    <!-- 两页并排，整条轨道平移：位移和高度都是原生 CSS 过渡 -->
    <div
      class="flex w-[200%]"
      style:transform="translateX({-50 * at}%)"
      style:transition={peeking ? 'none' : `transform ${PAGE_MS}ms ${ease}`}
    >
      <!-- 每一页**自己**带 12px 外边距（所以 Sheet 那边传了 padClass="px-0"）。
           挂在 Sheet 内容区上的话，两页交界处没有边距 —— 切换动画滑到中间时，
           左页的卡片和右页的卡片会贴成**一整条**（实测 x=12..399 连续无缝）。
           卡片带外边距，滑过去才像「整页替换」。 -->
      <div class="w-1/2 shrink-0 px-3">
        <div bind:this={paneModels} class="flex flex-col gap-3">
          <!-- 模型提供商：切到第二页，选看哪家的模型 -->
          <button
            onclick={() => openSub('providers')}
            class="flex w-full items-center gap-3 rounded-2xl bg-surface px-3.5 py-4 text-left"
          >
            <span class="shrink-0 text-[17px]">模型提供商</span>
            <span class="ml-auto flex min-w-0 items-center gap-0.5 text-[17px] text-muted">
              <span class="truncate">{shown?.name ?? '未配置'}</span>
              <Icon name="chevron" size={16} class="shrink-0" />
            </span>
          </button>

          <!-- 模型清单：自己滚，别把下面那块顶出屏幕 -->
          <!-- 卡片语言照 Claude 的「选择模型」面板：面板底色是 ink（#151515），
               卡片是 surface（#20201f），行之间是 **2px 的面板色** —— 不是 1px 的
               hairline，那 2px 实测就是 #151515，看着像「行之间有缝」。
               卡片自己的内边距是 14px（px-3.5）。 -->
          <div
            bind:this={scroller}
            class="max-h-[42dvh] overflow-y-auto overscroll-contain rounded-2xl bg-surface"
          >
            {#if !models.loaded}
              <p class="px-3.5 py-4 text-base text-muted">载入中…</p>
            {:else if !shown}
              <!-- 一家都没配：直接带去设置页 -->
              <button onclick={onconfig} class="flex w-full flex-col gap-0.5 px-3.5 py-4 text-left">
                <span class="text-[17px]">还没有配置模型提供商</span>
                <span class="text-meta text-accent">去设置里填一个 API Key</span>
              </button>
            {:else}
              {#if shown.error}
                <p class="border-b-2 border-ink px-3.5 py-3 text-meta leading-relaxed text-bad">
                  拉不到模型：{shown.error}
                </p>
              {/if}
              {#each shown.models as m (m.id)}
                <button
                  data-current={isCurrent(shown, m) ? '1' : undefined}
                  onclick={() => pickModel(shown, m)}
                  disabled={busy}
                  class="flex w-full items-center gap-3 border-b-2 border-ink px-3.5 py-3
                         text-left last:border-b-0"
                >
                  <span class="min-w-0 flex-1 truncate text-[17px]">{m.name}</span>
                  {#if isCurrent(shown, m)}
                    <Icon name="check" size={17} class="shrink-0 text-accent" />
                  {/if}
                </button>
              {/each}
            {/if}
          </div>

          <!-- 思考强度：切到第二页 -->
          <button
            onclick={() => openSub('effort')}
            class="flex w-full items-center rounded-2xl bg-surface px-3.5 py-4 text-left"
          >
            <span class="text-[17px]">思考强度</span>
            <span class="ml-auto flex items-center gap-0.5 text-[17px] text-muted">
              {effortLabel(effort)}
              <Icon name="chevron" size={16} />
            </span>
          </button>

        </div>
      </div>

      <div class="w-1/2 shrink-0 px-3">
        <div bind:this={paneSub} class="flex flex-col gap-3">
          {#if sub === 'providers'}
            <!-- 只列配好了 Key 的几家；加新的去设置页 -->
            {#if models.groups.length > 0}
              <div class="overflow-hidden rounded-2xl bg-surface">
                {#each models.groups as g (g.id)}
                  <button
                    onclick={() => view(g.id)}
                    class="flex w-full items-center gap-3 border-b-2 border-ink px-3.5 py-3
                           text-left last:border-b-0"
                  >
                    <span class="min-w-0 flex-1">
                      <span class="block truncate text-[17px]">{g.name}</span>
                      <span class="block truncate text-meta {g.error ? 'text-bad' : 'text-muted'}">
                        {g.error ? '拉不到模型' : g.id === currentProvider ? '正在使用' : `${g.models.length} 个模型`}
                      </span>
                    </span>
                    {#if g.id === shown?.id}
                      <Icon name="check" size={17} class="shrink-0 text-accent" />
                    {/if}
                  </button>
                {/each}
              </div>
            {/if}

            <button
              onclick={onconfig}
              class="flex w-full items-center gap-3 rounded-2xl bg-surface px-3.5 py-4 text-left"
            >
              <span class="text-[17px]">管理模型提供商</span>
              <Icon name="chevron" size={16} class="ml-auto shrink-0 text-muted" />
            </button>
          {:else}
            <div class="overflow-hidden rounded-2xl bg-surface">
              {#each EFFORTS as e (e.label)}
                <button
                  onclick={() => pickEffort(e.value)}
                  disabled={busy}
                  class="flex w-full items-center gap-3 border-b-2 border-ink px-3.5 py-3
                         text-left last:border-b-0"
                >
                  <span class="min-w-0 flex-1 truncate text-[17px]">{e.label}</span>
                  {#if e.value === effort}
                    <Icon name="check" size={17} class="shrink-0 text-accent" />
                  {/if}
                </button>
              {/each}
            </div>

            <p class="px-1 text-meta leading-relaxed text-muted">
              档位越高，模型想得越久：回复更充分，也更慢。有些服务不认识这个参数，会直接忽略。
            </p>
          {/if}
        </div>
      </div>
    </div>
  </div>

  {#if models.err}<p class="px-1 pt-3 text-xs text-bad">{models.err}</p>{/if}
</Sheet>
