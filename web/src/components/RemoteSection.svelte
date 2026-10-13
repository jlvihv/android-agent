<script lang="ts">
  /**
   * 设置页里的「连接电脑」：让这个 App 改连电脑上的核心（电脑上 `just dev-lan`）。
   *
   * 这一栏开着的时候在局域网里找电脑（mDNS，壳里的 CoreDiscovery），关掉就停 ——
   * 不常驻。点找到的那台：配对过的原生确认一下就连；第一次连要输电脑终端上显示
   * 的 6 位配对码（换回钥匙，之后不用再输）。找不到时还能手动粘地址。
   *
   * 切换、确认、配对都在壳里，而且都是**原生**对话框 —— 壳的 JS 接口每个 frame
   * 都看得见，不确认的话 AI 写的网页也能把 App 带走。
   *
   * 只在 App 里出现（`canSwitchCore`）：电脑浏览器、手机浏览器里没有壳，切不了。
   */
  import { isLoopbackHost } from '../lib/markdown';
  import {
    canSwitchCore,
    connectDiscoveredCore,
    connectRemoteCore,
    discoverCores,
    useLocalCore,
    type FoundCore,
  } from '../lib/overlay';

  const inApp = canSwitchCore();
  /** 现在连的是电脑：页面不是从 127.0.0.1 来的 */
  const remote = !isLoopbackHost(location.hostname);
  let cores = $state<FoundCore[]>([]);
  let url = $state('');

  // 这一栏挂着就找，卸掉（关设置页）就停
  $effect(() => {
    if (!inApp || remote) return;
    return discoverCores((c) => (cores = c));
  });
</script>

{#if inApp}
  <h2 class="px-1 pt-6 pb-2 text-meta text-muted">连接电脑</h2>
  <div class="overflow-hidden rounded-2xl bg-surface">
    {#if remote}
      <div class="flex items-center gap-3 px-3.5 py-3">
        <span class="min-w-0 flex-1">
          <span class="block text-[17px]">已连接到电脑</span>
          <span class="block truncate text-meta text-accent">{location.host}</span>
        </span>
        <button onclick={useLocalCore} class="shrink-0 py-1 text-[15px] text-muted">回到手机</button>
      </div>
    {:else}
      {#each cores as c (c.id)}
        <button
          onclick={() => connectDiscoveredCore(c.id)}
          class="flex w-full items-center gap-3 border-b-2 border-ink px-3.5 py-3 text-left"
        >
          <span class="min-w-0 flex-1">
            <span class="block truncate text-[17px]">{c.name}</span>
            <span class="block truncate text-meta text-muted">
              {c.host}:{c.port} · {c.paired ? '已配对' : '第一次连要输配对码'}
            </span>
          </span>
          <span class="shrink-0 text-[15px] text-accent">连接</span>
        </button>
      {/each}
      <p class="border-b-2 border-ink px-3.5 py-3 text-meta text-faint">
        {cores.length ? '还在找…' : '正在局域网里找电脑…'}
      </p>
      <details class="group px-3.5 py-3">
        <summary class="cursor-default list-none text-meta text-muted">
          <span class="inline-block transition-transform group-open:rotate-90">›</span>
          找不到？手动输入地址
        </summary>
        <div class="flex flex-col gap-3 pt-3">
          <input
            bind:value={url}
            inputmode="url"
            autocapitalize="off"
            autocomplete="off"
            spellcheck="false"
            placeholder="http://192.168.x.x:8787/auth?t=…"
            class="w-full rounded-xl bg-hairline-soft px-3 py-2.5 font-mono text-[13px] outline-none"
          />
          <button
            onclick={() => connectRemoteCore(url.trim())}
            disabled={!/^https?:\/\/\S+/.test(url.trim())}
            class="rounded-full bg-fg py-3 text-[15px] font-medium text-ink disabled:opacity-40"
          >
            连接
          </button>
        </div>
      </details>
    {/if}
  </div>
  <p class="px-1 pt-3 text-meta leading-relaxed text-faint">
    {#if remote}
      现在界面、命令、文件都在电脑上；手机自己的核心还在后台，回去是瞬间的。
    {:else}
      电脑上跑 <code class="font-mono">just dev-lan</code>，它会出现在上面；第一次连输它终端里显示的配对码。
      手机和电脑要在同一个 Wi-Fi 里；是明文 HTTP，只在自己信得过的网络里用。
    {/if}
  </p>
{/if}
