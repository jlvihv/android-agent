<script lang="ts" module>
  /** `GET /api/mcp/servers` 的一行。请求头只有名字：值（token）服务端从不回 */
  export type McpServer = {
    name: string;
    url: string;
    headers: string[];
    enabled: boolean;
  };
</script>

<script lang="ts">
  /**
   * 设置页的「MCP 服务」一栏：已接的远程 MCP 服务 + 添加入口。
   *
   * 只接远程（HTTP）服务，不在本地起进程（见 core/src/mcp.rs）。模型通过沙箱里的
   * `agent mcp` 用它们，一般也是让模型用 `agent mcp add` 接上的（这里给人看 / 手动改）。
   * 凭据存在核心里，之后由核心带着去请求。
   */
  import McpPage from "./McpPage.svelte";
  import Switch from "./Switch.svelte";

  let servers = $state<McpServer[]>([]);
  let err = $state("");
  /** 正在编辑哪一个；`'new'` = 添加 */
  let open = $state<McpServer | "new" | null>(null);

  async function load() {
    err = "";
    try {
      const r = await fetch("/api/mcp/servers");
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      servers = ((await r.json()) as { servers: McpServer[] }).servers;
    } catch (e) {
      err = String(e);
    }
  }

  $effect(() => {
    void load();
  });

  /** 停用的不进 AI 的提示词，AI 调它会被拒；配置留着 */
  async function setEnabled(s: McpServer, enabled: boolean) {
    err = "";
    try {
      const r = await fetch("/api/mcp/servers/enabled", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ name: s.name, enabled }),
      });
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      await load();
    } catch (e) {
      err = String(e);
    }
  }

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
    <!-- 名字那块点开编辑；开关单独一个（按钮里不能再套按钮） -->
    <div class="flex items-center gap-3 border-b-2 border-ink pr-3.5">
      <button
        onclick={() => (open = s)}
        class="min-w-0 flex-1 py-3 pl-3.5 text-left"
      >
        <span class="block truncate text-[17px] {s.enabled ? '' : 'text-muted'}"
          >{s.name}</span
        >
        <span
          class="block truncate text-meta {s.enabled
            ? 'text-muted'
            : 'text-faint'}"
        >
          {s.enabled ? "" : "已停用 · "}{host(s.url)}{s.headers.length
            ? " · 带凭据"
            : ""}
        </span>
      </button>
      <Switch
        checked={s.enabled}
        label="启用 {s.name}"
        onchange={(on) => setEnabled(s, on)}
      />
    </div>
  {/each}
  <button
    onclick={() => (open = "new")}
    class="flex w-full items-center gap-3 px-3.5 py-3 text-left"
  >
    <span class="flex-1 text-[17px] text-accent">添加 MCP 服务</span>
  </button>
</div>
<p class="px-1 pt-2 text-meta leading-relaxed text-faint">
  只接远程（HTTP）服务。把地址（和 token）发给 AI，它会用 <code
    >agent mcp add</code
  > 接上。
</p>
{#if err}<p class="px-1 pt-2 text-xs text-bad">{err}</p>{/if}

{#if open}
  <McpPage
    server={open === "new" ? null : open}
    onclose={() => (open = null)}
    onchange={load}
  />
{/if}
