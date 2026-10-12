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
   *   3. **几家服务商的模型都在这一个清单里**，按家分组（只配了一家就不显示组名）。
   *      点哪家的模型就切到哪家 —— Key 在设置页里各存一份。
   *   4. 最后那块是「模型服务」：打开设置页（配 Key 的地方）。
   *
   * 第二页没有返回箭头：安卓本来就有返回键/返回手势，再画一个纯属重复。
   * 返回键在 `Sheet` 里被接管（见它的 `onback`），按下是退回第一页。
   */
  import Icon from './Icon.svelte';
  import Sheet from './Sheet.svelte';
  import { store } from '../lib/store.svelte.ts';
  import { EFFORTS, effortLabel } from '../lib/effort';
  import { PAGE_MS } from '../lib/drag';
  import type { Peek } from '../lib/overlay';

  let {
    onclose,
    onconfig,
    rev = 0,
  }: {
    onclose: () => void;
    /** 「模型服务」→ 打开设置页（各家的 API Key） */
    onconfig: () => void;
    /** 设置页关掉时会变：Key 可能改了，清单要重拉 */
    rev?: number;
  } = $props();

  type Model = { id: string; name: string };
  /** 一家服务商的模型（`GET /api/models` 的 `groups`） */
  type Group = { id: string; name: string; models: Model[]; error?: string };
  type Page = 'models' | 'effort';

  let page = $state<Page>('models');
  let groups = $state<Group[]>([]);
  /** 当前在用的是哪家（服务商 id） */
  let currentProvider = $state<string | null>(null);
  const modelCount = $derived(groups.reduce((n, g) => n + g.models.length, 0));
  let loading = $state(true);
  let err = $state('');
  let busy = $state(false);
  let scroller = $state<HTMLDivElement | null>(null);
  let paneModels = $state<HTMLDivElement | null>(null);
  let paneEffort = $state<HTMLDivElement | null>(null);
  let hModels = $state(0);
  let hEffort = $state(0);

  const current = $derived(store.llm?.model ?? '');
  const effort = $derived(store.llm?.reasoningEffort ?? null);
  /** 预测性返回的进度（只在思考强度页）：0 = 原位，1 = 已完全回到第一页 */
  let peekP = $state(0);
  /** 跟手中：关掉过渡，不然手指和内容之间有延迟 */
  let peeking = $state(false);

  /** 0 = 在第一页，1 = 在第二页；跟手返回时介于两者之间 */
  const at = $derived(page === 'effort' ? 1 - peekP : 0);
  /** 视口高度跟着当前页走：切页时整个面板一起长高/变矮；跟手时按进度插值 */
  const height = $derived(hModels + (hEffort - hModels) * at);
  const ease = 'cubic-bezier(0.32,0.72,0,1)';

  const back = () => (page = 'models');

  /**
   * 思考强度页的跟手返回：第一页从左边跟着手指滑回来（和点进来时的方向相反），
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
  // 不受视口限制时的高度。`models.length` / `loading` 是依赖：清单到位后要重量。
  $effect(() => {
    void modelCount;
    void groups.length;
    void loading;
    if (paneModels) hModels = paneModels.offsetHeight;
    if (paneEffort) hEffort = paneEffort.offsetHeight;
  });

  $effect(() => {
    void rev;
    void load();
  });

  async function load() {
    loading = true;
    err = '';
    try {
      const r = await fetch('/api/models');
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      const d = (await r.json()) as { groups?: Group[]; provider?: string | null };
      groups = d.groups ?? [];
      currentProvider = d.provider ?? null;
    } catch (e) {
      err = String(e);
    } finally {
      loading = false;
    }
    // 38 个模型，当前那个多半在折叠线以下 —— 不滚过去的话，打开面板
    // 只看得到一屏陌生名字，还得自己找。下一帧再滚（行还没渲染完）。
    requestAnimationFrame(() => {
      scroller?.querySelector('[data-current="1"]')?.scrollIntoView({ block: 'center' });
    });
  }

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
    err = '';
    try {
      const r = await fetch('/api/model', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ model, provider: provider ?? undefined, reasoningEffort: e ?? '' }),
      });
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      currentProvider = provider;
      await store.reloadHealth();
    } catch (e) {
      err = String(e);
    } finally {
      busy = false;
    }
  }
</script>

<Sheet
  title={page === 'effort' ? '思考强度' : '选择模型'}
  onback={page === 'effort' ? back : undefined}
  {peek}
  slide={page === 'effort' ? 1 : -1}
  minClass="min-h-0"
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
      <div class="w-1/2 shrink-0">
        <div bind:this={paneModels} class="flex flex-col gap-3">
          <!-- 模型清单：自己滚，别把下面两块顶出屏幕 -->
          <div
            bind:this={scroller}
            class="max-h-[42dvh] overflow-y-auto overscroll-contain rounded-2xl bg-bubble"
          >
            {#if loading && groups.length === 0}
              <p class="px-4 py-3.5 text-base text-muted">载入中…</p>
            {:else if groups.length === 0}
              <!-- 一家都没配：直接带去设置页 -->
              <button onclick={onconfig} class="flex w-full flex-col gap-0.5 px-4 py-3.5 text-left active:bg-control">
                <span class="text-[17px]">还没有配置模型服务</span>
                <span class="text-meta text-accent">去设置里填一个 API Key</span>
              </button>
            {:else}
              {#each groups as g (g.id)}
                <!-- 只配了一家就不显示组名（就是一个普通清单） -->
                {#if groups.length > 1}
                  <p class="sticky top-0 z-10 bg-bubble px-4 pt-3 pb-1.5 text-meta text-muted">{g.name}</p>
                {/if}
                {#if g.error}
                  <p class="border-b border-hairline px-4 py-3 text-meta leading-relaxed text-bad">
                    拉不到模型：{g.error}
                  </p>
                {/if}
                {#each g.models as m (m.id)}
                  <button
                    data-current={isCurrent(g, m) ? '1' : undefined}
                    onclick={() => pickModel(g, m)}
                    disabled={busy}
                    class="flex w-full items-center gap-3 border-b border-hairline px-4 py-3.5
                           text-left active:bg-control"
                  >
                    <span class="min-w-0 flex-1 truncate text-[17px]">{m.name}</span>
                    {#if isCurrent(g, m)}
                      <Icon name="check" size={17} class="shrink-0 text-fg" />
                    {/if}
                  </button>
                {/each}
              {/each}
            {/if}
          </div>

          <!-- 思考强度：切到第二页 -->
          <button
            onclick={() => (page = 'effort')}
            class="flex w-full items-center rounded-2xl bg-bubble px-4 py-3.5 text-left
                   active:bg-control"
          >
            <span class="text-[17px]">思考强度</span>
            <span class="ml-auto flex items-center gap-0.5 text-[17px] text-muted">
              {effortLabel(effort)}
              <Icon name="chevron" size={16} />
            </span>
          </button>

          <!-- 模型服务：打开设置页（各家的 API Key） -->
          <button
            onclick={onconfig}
            class="flex w-full items-center gap-3 rounded-2xl bg-bubble px-4 py-3.5 text-left
                   active:bg-control"
          >
            <span class="text-[17px]">模型服务</span>
            <span class="ml-auto min-w-0 truncate text-[17px] text-muted">
              {groups.length > 0 ? `${groups.length} 家` : '未配置'}
            </span>
            <Icon name="chevron" size={16} class="shrink-0 text-muted" />
          </button>
        </div>
      </div>

      <div class="w-1/2 shrink-0">
        <div bind:this={paneEffort} class="flex flex-col gap-3">
          <div class="overflow-hidden rounded-2xl bg-bubble">
            {#each EFFORTS as e (e.label)}
              <button
                onclick={() => pickEffort(e.value)}
                disabled={busy}
                class="flex w-full items-center gap-3 border-b border-hairline px-4 py-3.5
                       text-left last:border-b-0 active:bg-control"
              >
                <span class="min-w-0 flex-1 truncate text-[17px]">{e.label}</span>
                {#if e.value === effort}
                  <Icon name="check" size={17} class="shrink-0 text-fg" />
                {/if}
              </button>
            {/each}
          </div>

          <p class="px-1 text-meta leading-relaxed text-muted">
            档位越高，模型想得越久：回复更充分，也更慢。有些服务不认识这个参数，会直接忽略。
          </p>
        </div>
      </div>
    </div>
  </div>

  {#if err}<p class="px-1 pt-3 text-xs text-bad">{err}</p>{/if}
</Sheet>
