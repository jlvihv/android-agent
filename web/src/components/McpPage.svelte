<script lang="ts">
  /**
   * 添加 / 编辑一个远程 MCP 服务：名字、URL、可选的 token（作为 `Authorization: Bearer …`
   * 发出去）。保存后核心会试着列一次工具：连得上就报工具数、连不上照样存着并说原因
   * （可能只是这会儿没网）。
   */
  import Page from './Page.svelte';
  import type { McpServer } from './McpSection.svelte';

  let {
    server,
    onclose,
    onchange,
  }: {
    /** `null` = 添加新的 */
    server: McpServer | null;
    onclose: () => void;
    onchange: () => Promise<void> | void;
  } = $props();

  // 只是初始值：之后用户自己改
  // svelte-ignore state_referenced_locally
  let name = $state(server?.name ?? '');
  // svelte-ignore state_referenced_locally
  let url = $state(server?.url ?? '');
  let token = $state('');
  let busy = $state(false);
  let err = $state('');
  /** 存好了、但连不上（存着，说一声原因） */
  let warn = $state('');
  let page: Page | null = $state(null);

  const NAME_RE = /^[a-z0-9_-]+$/;
  const nameOk = $derived(NAME_RE.test(name.trim()) && name.trim() !== 'list');
  const canSave = $derived(!busy && nameOk && /^https?:\/\//.test(url.trim()));

  async function save() {
    if (!canSave) return;
    busy = true;
    err = '';
    warn = '';
    try {
      const t = token.trim();
      const body: Record<string, unknown> = { name: name.trim(), url: url.trim() };
      // 编辑时不填 token = 沿用原来的凭据（不发 headers 字段）
      if (t) body.headers = { Authorization: /^bearer\s/i.test(t) ? t : `Bearer ${t}` };
      else if (!server) body.headers = {};
      const r = await fetch('/api/mcp/servers', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(body),
      });
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      const res = (await r.json()) as { ok: boolean; tools?: number; error?: string };
      await onchange();
      if (res.ok) page?.dismiss();
      else warn = `已保存，但现在连不上：${res.error ?? ''}`;
    } catch (e) {
      err = String(e).replace(/^Error: /, '');
    } finally {
      busy = false;
    }
  }

  async function remove() {
    if (!server) return;
    busy = true;
    err = '';
    try {
      const r = await fetch('/api/mcp/servers/delete', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ name: server.name }),
      });
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      await onchange();
      page?.dismiss();
    } catch (e) {
      err = String(e).replace(/^Error: /, '');
    } finally {
      busy = false;
    }
  }

  const input =
    'rounded-2xl bg-surface px-4 py-3.5 text-[17px] outline-none placeholder:text-faint disabled:text-muted';
</script>

<Page bind:this={page} title={server ? server.name : '添加 MCP 服务'} {onclose}>
  <div class="flex flex-col gap-4 pt-2">
    <label class="flex flex-col gap-1.5">
      <span class="px-1 text-meta text-muted">名字</span>
      <input
        bind:value={name}
        disabled={!!server}
        placeholder="github"
        autocapitalize="off"
        autocorrect="off"
        spellcheck="false"
        class={input}
      />
      <span class="px-1 text-meta text-faint {name && !nameOk ? 'text-bad' : ''}">
        AI 用 <code>agent mcp {name.trim() || '<名字>'} …</code> 调它：小写字母、数字、- 和 _
      </span>
    </label>

    <label class="flex flex-col gap-1.5">
      <span class="px-1 text-meta text-muted">URL</span>
      <input
        bind:value={url}
        placeholder="https://…/mcp"
        autocapitalize="off"
        autocorrect="off"
        spellcheck="false"
        inputmode="url"
        class={input}
      />
      <span class="px-1 text-meta text-faint">远程 MCP 端点（Streamable HTTP）</span>
    </label>

    <label class="flex flex-col gap-1.5">
      <span class="px-1 text-meta text-muted">Token（可选）</span>
      <input
        bind:value={token}
        type="password"
        placeholder={server?.headers.length ? '已保存，填新的会替换' : '作为 Authorization: Bearer 发送'}
        autocapitalize="off"
        autocorrect="off"
        spellcheck="false"
        onkeydown={(e) => e.key === 'Enter' && save()}
        class={input}
      />
    </label>

    {#if err}<p class="px-1 text-xs leading-relaxed text-bad">{err}</p>{/if}
    {#if warn}<p class="px-1 text-xs leading-relaxed text-warn">{warn}</p>{/if}

    <button
      onclick={save}
      disabled={!canSave}
      class="rounded-full bg-fg py-3.5 text-[15px] font-medium text-ink disabled:opacity-40"
    >
      {busy ? '连接中…' : '保存'}
    </button>

    {#if server}
      <button onclick={remove} disabled={busy} class="py-2 text-[15px] text-bad disabled:opacity-40">
        删除这个服务
      </button>
    {/if}
  </div>
</Page>
