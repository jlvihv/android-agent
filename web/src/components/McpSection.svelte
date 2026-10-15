<script lang="ts" module>
  /** `GET /api/mcp/servers` 的一行。请求头只有名字：值（token）服务端从不回 */
  export type McpServer = { name: string; url: string; headers: string[] };
</script>

<script lang="ts">
  /**
   * 设置页的「MCP 服务」一栏：已接的远程 MCP 服务 + 添加入口。
   *
   * 只接远程（HTTP）服务，不在本地起进程（见 core/src/mcp.rs）。模型通过沙箱里的
   * `agent mcp` 用它们，一般也是让模型用 `agent mcp add` 接上的（这里给人看 / 手动改）。
   * 凭据存在核心里，之后由核心带着去请求。
   */
  import Icon from './Icon.svelte';
  import McpPage from './McpPage.svelte';

  let servers = $state<McpServer[]>([]);
  let err = $state('');
  /** 正在编辑哪一个；`'new'` = 添加 */
  let open = $state<McpServer | 'new' | null>(null);

  async function load() {
    err = '';
    try {
      const r = await fetch('/api/mcp/servers');
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      servers = ((await r.json()) as { servers: McpServer[] }).servers;
    } catch (e) {
      err = String(e);
    }
  }

  $effect(() => {
    void load();
  });

  const host = (url: string) => {
    try {
      return new URL(url).host;
    } catch {
      return url;
    }
  };
</script>

<h2 class="px-1 pt-6 pb-2 text-meta text-muted">MCP 服务</h2>
<div class="overflow-hidden rounded-2xl bg-surface">
  {#each servers as s (s.name)}
    <button
      onclick={() => (open = s)}
      class="flex w-full items-center gap-3 border-b-2 border-ink px-3.5 py-3 text-left"
    >
      <span class="min-w-0 flex-1">
        <span class="block truncate text-[17px]">{s.name}</span>
        <span class="block truncate text-meta text-muted">
          {host(s.url)}{s.headers.length ? ' · 带凭据' : ''}
        </span>
      </span>
      <Icon name="chevron" size={16} class="shrink-0 text-muted" />
    </button>
  {/each}
  <button onclick={() => (open = 'new')} class="flex w-full items-center gap-3 px-3.5 py-3 text-left">
    <span class="flex-1 text-[17px] text-accent">添加 MCP 服务</span>
  </button>
</div>
<p class="px-1 pt-2 text-meta leading-relaxed text-faint">
  只接远程（HTTP）服务。把地址（和 token）发给 AI，它会用 <code>agent mcp add</code> 接上。
</p>
{#if err}<p class="px-1 pt-2 text-xs text-bad">{err}</p>{/if}

{#if open}
  <McpPage server={open === 'new' ? null : open} onclose={() => (open = null)} onchange={load} />
{/if}
