<script lang="ts">
  /**
   * 「选择机器」面板：本机自己的核心 ↔ 局域网里／经服务器的哪台电脑，就地切换。
   *
   * 侧栏底栏那个小标签（`machine`）点开就是它 —— 底栏写着「现在连的是哪台」，
   * 点开就地换一台。形状和「选择模型」一致：点标签 → 底部面板 → 就地换。
   * 这是换机器的**唯一**入口（设置页里不再有「连接电脑」那一栏）。
   *
   * 数据来自本机核心（`GET /api/machines`，见 `lib/machines.svelte.ts`）——
   * 面板在哪个页面上打开都一样：经隧道时隧道把那份请求转回本机（见 docs/12），
   * 所以列出来的永远是**你自己这台机器**的配对和发现结果。
   *
   * 局域网里那几台是本机的发现服务报进来的：手机上是壳的 `NsdManager`，电脑上是核心
   * 自己起的 `avahi-browse` —— 页面这边只管问，两边都是「面板开着才找」（见 docs/11）。
   *
   * 要输配对码的那一页走 `Sheet` 的 `onback`：系统返回键在第二页是退回列表，而不是把
   * 面板关掉（和「选择模型 → 思考强度」一样）。电脑上还多一个「手输地址」的口子
   * （广播工具不在、`avahi-daemon` 没跑时用）—— 手机上不给：页面能传任意地址的话，
   * 查看器里 AI 写的网页就能把界面带到别人的服务器上（见 docs/11）。
   */
  import Icon from './Icon.svelte';
  import Sheet from './Sheet.svelte';
  import { hasShell, machines, type Machine } from '../lib/machines.svelte.ts';

  let { onclose }: { onclose: () => void } = $props();

  type Page = 'list' | 'pair' | 'manual';
  let page = $state<Page>('list');
  /** 要配的那台（那一页用它显示名字）；手输地址时是 null */
  let target = $state<Machine | null>(null);
  let code = $state('');
  let host = $state('');
  let port = $state('8787');
  /** 配对码输入框：进那一页就聚焦（省一次点） */
  let input = $state<HTMLInputElement | null>(null);

  // 挂着就一直问（发现在壳 / 核心那边推进来，见 `machines.watch`）
  $effect(() => machines.watch());

  $effect(() => {
    if (page === 'pair') input?.focus();
  });

  function ask(m: Machine) {
    target = m;
    code = '';
    machines.err = '';
    page = 'pair';
  }

  /**
   * 配对：输它显示的 6 位码（核心那边会接着把隧道也起好）。
   *
   * 成功**不关面板**：这一步已经开着往另一个地址的导航了，面板跟着这个页面一起消失。
   * 手动 `onclose()` 反而会把导航打掉 —— 覆盖层关闭时会 `history.back()`（见
   * `lib/overlay.ts` 的 `onBack`），而它正好取消掉那条还没落地的导航。
   * 失败才留着：那行原因就在下面。
   */
  async function submit() {
    const t = target;
    const c = code.trim();
    if (!t || c.length < 6) return;
    await machines.pair(t.id, c);
  }

  /** 手输地址配对（只有电脑上有这个入口）。 */
  async function submitManual() {
    const p = Number(port);
    if (!host.trim() || !p || code.trim().length < 6) return;
    await machines.pairManual(host.trim(), p, code.trim());
  }
</script>

<!-- 一行机器。当前那台不画成按钮（点它等于什么也不做），勾在右边，和「选择模型」一致。 -->
{#snippet row(
  icon: 'phone' | 'computer' | 'globe',
  name: string,
  sub: string,
  current: boolean,
  onpick: () => void,
)}
  <svelte:element
    this={current ? 'div' : 'button'}
    type={current ? undefined : 'button'}
    role={current ? 'group' : undefined}
    onclick={current ? undefined : onpick}
    class="flex w-full items-center gap-3 border-b-2 border-ink px-3.5 py-3 text-left"
  >
    <Icon name={icon} size={18} class="shrink-0 text-muted" />
    <span class="min-w-0 flex-1">
      <span class="block truncate text-[17px]">{name}</span>
      <span class="block truncate text-meta text-muted">{sub}</span>
    </span>
    {#if current}<Icon name="check" size={16} class="shrink-0 text-accent" />{/if}
  </svelte:element>
{/snippet}

<!-- `hug`：就这几行，不撑到 45% 屏高（和「上下文用量」那个面板一样）。 -->
<Sheet
  title={page === 'pair' ? '输配对码' : page === 'manual' ? '手输地址' : '选择机器'}
  hug
  {onclose}
  onback={page === 'list' ? undefined : () => (page = 'list')}
>
  {#if page === 'list'}
    <div class="overflow-hidden rounded-2xl bg-surface">
      <!-- 本机永远是第一个：它就是「另一头」，连着电脑时点它回去。 -->
      {@render row(
        hasShell() ? 'phone' : 'computer',
        hasShell() ? '这台手机' : '这台电脑',
        machines.current
          ? '回到自己的核心（界面、命令、文件都在这台上）'
          : '界面、命令、文件都在这台上',
        machines.current === null,
        () => machines.useLocal(),
      )}

      {#each machines.list as m (m.id)}
        {@render row(
          m.relay ? 'globe' : 'computer',
          m.name,
          m.relay
            ? '经服务器连（不在同一个 Wi-Fi 里）'
            : m.current
              ? `${m.host}:${m.port}`
              : `${m.host}:${m.port} · ${m.paired ? '已配对' : '第一次连要输配对码'}`,
          m.current,
          () => (m.paired ? machines.connect(m.id) : ask(m)),
        )}
      {/each}

      {#if !hasShell()}
        <button
          type="button"
          onclick={() => {
            machines.err = '';
            host = '';
            port = '8787';
            code = '';
            page = 'manual';
          }}
          class="flex w-full items-center gap-3 px-3.5 py-3 text-left"
        >
          <Icon name="plus" size={18} class="shrink-0 text-muted" />
          <span class="text-[17px] text-muted">手输地址…</span>
        </button>
      {/if}
    </div>

    {#if machines.err}
      <p class="px-3.5 pt-3 text-meta leading-relaxed text-red-400">{machines.err}</p>
    {/if}

    <p class="px-3.5 py-3 text-meta text-faint">
      {machines.list.length ? '还在找…' : '正在局域网里找电脑…'}
    </p>
    <p class="px-1 text-meta leading-relaxed text-faint">
      {#if hasShell()}
        电脑上跑 <code class="font-mono">just dev-host</code>，它会出现在上面；第一次要在同一个
        Wi-Fi 里配对。电脑配了中继的话，出门在外它也在这里（经服务器，端到端加密）。
      {:else}
        别的电脑上跑 <code class="font-mono">just dev-host</code>，它会出现在上面；第一次要在同一个
        Wi-Fi 里、输一次它终端里那个 6 位码。找不到（<code class="font-mono">avahi-daemon</code
        > 没跑？）就手输地址。
      {/if}
    </p>
  {:else if page === 'pair'}
    <div class="rounded-2xl bg-surface p-3.5">
      <p class="text-meta leading-relaxed text-muted">
        在「{target?.name}」上看那个 6 位码（终端启动那几行，或者它设置页里的「连接手机」）。
      </p>
      <input
        bind:this={input}
        bind:value={code}
        inputmode="numeric"
        maxlength="6"
        placeholder="6 位配对码"
        class="mt-3 w-full rounded-xl bg-ink px-3 py-2 text-center font-mono text-[22px] tracking-[0.3em]"
        onkeydown={(e) => e.key === 'Enter' && code.trim().length >= 6 && submit()}
      />
      {#if machines.err}
        <p class="pt-2 text-meta leading-relaxed text-red-400">{machines.err}</p>
      {/if}
      <button
        type="button"
        disabled={code.trim().length < 6 || machines.busy}
        onclick={submit}
        class="mt-3 w-full rounded-xl bg-accent py-2.5 text-[17px] font-medium text-ink disabled:opacity-40"
      >
        {machines.busy ? '配对中…' : '配对并连上'}
      </button>
    </div>
    <p class="px-1 pt-3 text-meta leading-relaxed text-faint">
      只要输这一次：配对之后这台机器记着它的凭据和证书指纹，下次点一下就连。
    </p>
  {:else}
    <div class="rounded-2xl bg-surface p-3.5">
      <p class="text-meta leading-relaxed text-muted">
        能被找到就不用走这里。地址是那台电脑上核心的地址（<code class="font-mono"
          >just dev-host</code
        > 启动时打过），配对码在它终端里。
      </p>
      <div class="space-y-3 pt-3">
        <label class="block">
          <span class="block px-1 pb-1 text-meta text-muted">地址</span>
          <!-- svelte-ignore a11y_autofocus -->
          <input
            bind:value={host}
            placeholder="192.168.0.9"
            autocapitalize="off"
            autocomplete="off"
            spellcheck="false"
            class="w-full rounded-xl bg-ink px-3 py-2 font-mono text-[17px]"
          />
        </label>
        <label class="block">
          <span class="block px-1 pb-1 text-meta text-muted">端口</span>
          <input
            bind:value={port}
            inputmode="numeric"
            placeholder="8787"
            class="w-full rounded-xl bg-ink px-3 py-2 font-mono text-[17px]"
          />
        </label>
        <label class="block">
          <span class="block px-1 pb-1 text-meta text-muted">配对码</span>
          <input
            bind:value={code}
            inputmode="numeric"
            maxlength="6"
            placeholder="6 位"
            class="w-full rounded-xl bg-ink px-3 py-2 font-mono text-[17px]"
            onkeydown={(e) => e.key === 'Enter' && submitManual()}
          />
        </label>
      </div>
      {#if machines.err}
        <p class="pt-2 text-meta leading-relaxed text-red-400">{machines.err}</p>
      {/if}
      <button
        type="button"
        disabled={!host.trim() || !Number(port) || code.trim().length < 6 || machines.busy}
        onclick={submitManual}
        class="mt-3 w-full rounded-xl bg-accent py-2.5 text-[17px] font-medium text-ink disabled:opacity-40"
      >
        {machines.busy ? '配对中…' : '配对并连上'}
      </button>
    </div>
  {/if}
</Sheet>
