<script lang="ts">
  /**
   * 「选择机器」面板：手机自己的核心 ↔ 局域网里某台电脑上的核心，就地切换。
   *
   * 侧栏底栏那个小标签（`machine`）点开就是它 —— 底栏写着「现在连的是哪台」，
   * 点开就地换一台。形状和「选择模型」一致：点药丸 → 底部面板 → 就地换。
   * 这是换机器的**唯一**入口（设置页里不再有「连接电脑」那一栏）。
   *
   * 找电脑是壳的 `CoreDiscovery`（面板挂着才找、关掉就停，见 `lib/overlay.ts` 的
   * `discoverCores`）。点了直接切，不再确认；第一次连的那台要输配对码 —— 那是壳的
   * **原生**对话框（页面给不了任意地址，只能点「真的找到过」的 id）。
   *
   * 只在 App 里出现（`canSwitchCore()`）：电脑浏览器、手机浏览器里没有壳，
   * 底栏那个标签就不是按钮（见 `Drawer.svelte`）。
   */
  import Icon from './Icon.svelte';
  import Sheet from './Sheet.svelte';
  import { store } from '../lib/store.svelte.ts';
  import {
    connectDiscoveredCore,
    discoverCores,
    useLocalCore,
    type FoundCore,
  } from '../lib/overlay';

  let { onclose }: { onclose: () => void } = $props();

  /** 局域网里找到的电脑（壳推过来的，见 `MainActivity.pushCoresFound`） */
  let cores = $state<FoundCore[]>([]);

  $effect(() => discoverCores((c) => (cores = c)));
</script>

<!-- 一行机器。当前那台不画成按钮（点它等于什么也不做），勾在右边，和「选择模型」一致。 -->
{#snippet row(
  icon: 'phone' | 'computer',
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
<Sheet title="选择机器" hug {onclose}>
  <div class="overflow-hidden rounded-2xl bg-surface">
    <!-- 这台手机永远排第一个：它就是「另一头」，连着电脑时点它回去。 -->
    {@render row(
      'phone',
      '这台手机',
      store.machine.phone ? '界面、命令、文件都在这台手机上' : '回到自己的核心（不用 Wi-Fi）',
      store.machine.phone,
      useLocalCore,
    )}

    {#each cores as c (c.id)}
      {@render row(
        'computer',
        c.name,
        c.current
          ? `${c.host}:${c.port}`
          : `${c.host}:${c.port} · ${c.paired ? '已配对' : '第一次连要输配对码'}`,
        c.current,
        () => connectDiscoveredCore(c.id),
      )}
    {/each}

    <p class="px-3.5 py-3 text-meta text-faint">
      {cores.length ? '还在找…' : '正在局域网里找电脑…'}
    </p>
  </div>
  <p class="px-1 pt-3 text-meta leading-relaxed text-faint">
    电脑上跑 <code class="font-mono">just dev-host</code>，它会出现在上面。手机和电脑要在同一个
    Wi-Fi 里；是明文 HTTP，只在自己信得过的网络里用。
  </p>
</Sheet>
