<script lang="ts">
  /**
   * 设置页里的「连接电脑」：让这个 App 改连电脑上的核心（电脑上 `just dev-lan`）。
   *
   * 切换本身在壳里（MainActivity）：连接前弹**原生**确认框 —— 壳的 JS 接口每个
   * frame 都看得见，不确认的话 AI 写的网页也能把 App 带走。选择记在壳那边，
   * 下次启动直接连；连不上时壳给原生错误页（重试 / 回到手机）。
   *
   * 只在 App 里出现（`canSwitchCore`）：电脑浏览器、手机浏览器里没有壳，切不了。
   */
  import { isLoopbackHost } from '../lib/markdown';
  import { canSwitchCore, connectRemoteCore, useLocalCore } from '../lib/overlay';

  const inApp = canSwitchCore();
  /** 现在连的是电脑：页面不是从 127.0.0.1 来的 */
  const remote = !isLoopbackHost(location.hostname);
  let url = $state('');
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
      <div class="flex flex-col gap-3 px-3.5 py-3">
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
    {/if}
  </div>
  <p class="px-1 pt-3 text-meta leading-relaxed text-faint">
    {#if remote}
      现在界面、命令、文件都在电脑上；手机自己的核心还在后台，回去是瞬间的。
    {:else}
      电脑上跑 <code class="font-mono">just dev-lan</code>，把它打出来的「手机上打开」那一整行地址粘进来。
      手机和电脑要在同一个 Wi-Fi 里；是明文 HTTP，只在自己信得过的网络里用。
    {/if}
  </p>
{/if}
