<script lang="ts">
  /**
   * 「选择机器」面板：手机自己的核心 ↔ 局域网里某台电脑上的核心，就地切换。
   *
   * 侧栏底栏那个小标签（`machine`）点开就是它 —— 底栏写着「现在连的是哪台」，
   * 点开就地换一台。形状和「选择模型」一致：点药丸 → 底部面板 → 就地换。
   * 不做的话，换机器得进设置页再滚到「连接电脑」那一栏，而「我在跟谁说话」和
   * 「怎么换成别的」本来就是一件事的两半。
   *
   * 内容和设置页那一栏**同源**：找电脑是壳的 `CoreDiscovery`（面板挂着才找、
   * 关掉就停，见 `lib/overlay.ts` 的 `discoverCores`），配对 / 确认还是壳的**原生**
   * 对话框（页面给不了任意地址，只能点「真的找到过」的 id）。
   *
   * 广播工具不在、网络隔离多播时一台也找不到 —— 所以底下留了一条
   * 「手动输入地址」通到设置页（设置页**叠在面板上面**，关掉它回到这个面板，
   * 跟模型面板里「管理模型提供商」一个走法）。
   *
   * 只在 App 里出现（`canSwitchCore()`）：电脑浏览器、手机浏览器里没有壳，
   * 底栏那个标签就不是按钮（见 `Drawer.svelte`）。
   */
  import Icon from './Icon.svelte';
  import Sheet from './Sheet.svelte';
  import { isLoopbackHost } from '../lib/host';
  import {
    connectDiscoveredCore,
    discoverCores,
    useLocalCore,
    type FoundCore,
  } from '../lib/overlay';

  let {
    onclose,
    onsettings,
  }: {
    onclose: () => void;
    /** 去「手动输入地址」（设置页的「连接电脑」那一栏） */
    onsettings: () => void;
  } = $props();

  /** 现在连的是电脑：页面不是从 127.0.0.1 来的 */
  const remote = !isLoopbackHost(location.hostname);
  /** 局域网里找到的电脑（壳推过来的，见 `MainActivity.pushCoresFound`） */
  let cores = $state<FoundCore[]>([]);

  $effect(() => discoverCores((c) => (cores = c)));
</script>

<!-- `hug`：就这几行，不撑到 45% 屏高（和「上下文用量」那个面板一样）。 -->
<Sheet title="选择机器" hug {onclose}>
  <div class="overflow-hidden rounded-2xl bg-surface">
    <!-- 这台手机永远排第一个：它就是「另一头」，连着电脑时点它回去。 -->
    {#if remote}
      <button
        onclick={useLocalCore}
        class="flex w-full items-center gap-3 border-b-2 border-ink px-3.5 py-3 text-left"
      >
        <Icon name="phone" size={18} class="shrink-0 text-muted" />
        <span class="min-w-0 flex-1">
          <span class="block truncate text-[17px]">这台手机</span>
          <span class="block truncate text-meta text-muted">回到自己的核心（不用 Wi-Fi）</span>
        </span>
      </button>
    {:else}
      <!-- 当前那个不画成按钮：点它等于什么也不做。勾在右边，和「选择模型」一致。 -->
      <div class="flex items-center gap-3 border-b-2 border-ink px-3.5 py-3">
        <Icon name="phone" size={18} class="shrink-0 text-muted" />
        <span class="min-w-0 flex-1">
          <span class="block truncate text-[17px]">这台手机</span>
          <span class="block truncate text-meta text-muted">界面、命令、文件都在这台手机上</span>
        </span>
        <Icon name="check" size={16} class="shrink-0 text-accent" />
      </div>
    {/if}

    {#each cores as c (c.id)}
      {#if c.current}
        <div class="flex items-center gap-3 border-b-2 border-ink px-3.5 py-3">
          <Icon name="computer" size={18} class="shrink-0 text-muted" />
          <span class="min-w-0 flex-1">
            <span class="block truncate text-[17px]">{c.name}</span>
            <span class="block truncate text-meta text-muted">{c.host}:{c.port}</span>
          </span>
          <Icon name="check" size={16} class="shrink-0 text-accent" />
        </div>
      {:else}
        <!-- 点了之后是壳的原生确认框（或输配对码），取消就还在这张面板上。
             连上了 WebView 会换页 —— 这边什么都不用做。 -->
        <button
          onclick={() => connectDiscoveredCore(c.id)}
          class="flex w-full items-center gap-3 border-b-2 border-ink px-3.5 py-3 text-left"
        >
          <Icon name="computer" size={18} class="shrink-0 text-muted" />
          <span class="min-w-0 flex-1">
            <span class="block truncate text-[17px]">{c.name}</span>
            <span class="block truncate text-meta text-muted">
              {c.host}:{c.port} · {c.paired ? '已配对' : '第一次连要输配对码'}
            </span>
          </span>
        </button>
      {/if}
    {/each}

    <p class="px-3.5 py-3 text-meta text-faint">
      {cores.length ? '还在找…' : '正在局域网里找电脑…'}
    </p>
  </div>

  <button
    onclick={onsettings}
    class="mt-1 flex w-full items-center gap-2 rounded-2xl px-3.5 py-3 text-left text-meta text-muted"
  >
    找不到？去设置里手动输入地址
    <Icon name="chevron" size={16} class="ml-auto shrink-0" />
  </button>
</Sheet>
