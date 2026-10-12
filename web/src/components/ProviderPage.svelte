<script lang="ts">
  /**
   * 一家服务商：填 API Key（「其他」还要填 Base URL），保存时服务端先拉一次
   * `/models` 验证，不对就不存。存好就回到设置页。
   */
  import Page from './Page.svelte';
  import type { ProviderRow } from './Settings.svelte';

  let {
    provider,
    onclose,
    onchange,
  }: {
    provider: ProviderRow;
    onclose: () => void;
    /** 存了 / 删了：让设置页刷新列表 */
    onchange: () => Promise<void> | void;
  } = $props();

  let apiKey = $state('');
  // 只是初始值：之后用户自己改
  // svelte-ignore state_referenced_locally
  let baseUrl = $state(provider.custom ? provider.baseUrl : '');
  let busy = $state(false);
  let err = $state('');
  let page: Page | null = $state(null);

  const canSave = $derived(
    !busy &&
      (apiKey.trim().length > 0 || (provider.configured && provider.custom)) &&
      (!provider.custom || baseUrl.trim().length > 0),
  );

  async function save() {
    if (!canSave) return;
    busy = true;
    err = '';
    try {
      const r = await fetch(`/api/providers/${provider.id}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ apiKey: apiKey.trim(), baseUrl: baseUrl.trim() }),
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

  async function remove() {
    busy = true;
    err = '';
    try {
      const r = await fetch(`/api/providers/${provider.id}`, { method: 'DELETE' });
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      await onchange();
      page?.dismiss();
    } catch (e) {
      err = String(e).replace(/^Error: /, '');
    } finally {
      busy = false;
    }
  }
</script>

<Page bind:this={page} title={provider.name} {onclose}>
  <div class="flex flex-col gap-4 pt-2">
    {#if provider.custom}
      <label class="flex flex-col gap-1.5">
        <span class="px-1 text-meta text-muted">Base URL</span>
        <input
          bind:value={baseUrl}
          placeholder="https://…/v1"
          autocapitalize="off"
          autocorrect="off"
          spellcheck="false"
          inputmode="url"
          class="rounded-2xl bg-surface px-4 py-3.5 text-[17px] outline-none placeholder:text-faint"
        />
        <span class="px-1 text-meta text-faint">任何 OpenAI 兼容的端点：Ollama、中转站、公司网关…</span>
      </label>
    {/if}

    <label class="flex flex-col gap-1.5">
      <span class="px-1 text-meta text-muted">API Key</span>
      <input
        bind:value={apiKey}
        type="password"
        placeholder={provider.configured ? `已保存 ${provider.keyHint ?? ''}，填新的会替换` : 'sk-…'}
        autocapitalize="off"
        autocorrect="off"
        spellcheck="false"
        onkeydown={(e) => e.key === 'Enter' && save()}
        class="rounded-2xl bg-surface px-4 py-3.5 text-[17px] outline-none placeholder:text-faint"
      />
      {#if provider.keyUrl}
        <!-- 外链：WebView 会把它交给系统浏览器（MainActivity.shouldOverrideUrlLoading） -->
        <a href={provider.keyUrl} class="px-1 text-meta text-accent">去 {provider.name} 获取 API Key</a>
      {/if}
    </label>

    {#if err}<p class="px-1 text-xs leading-relaxed text-bad">{err}</p>{/if}

    <button
      onclick={save}
      disabled={!canSave}
      class="rounded-full bg-fg py-3.5 text-[15px] font-medium text-ink disabled:opacity-40"
    >
      {busy ? '验证中…' : '保存'}
    </button>

    {#if provider.configured}
      <button onclick={remove} disabled={busy} class="py-2 text-[15px] text-bad disabled:opacity-40">
        删除这个 Key
      </button>
    {/if}

    <p class="px-1 text-meta leading-relaxed text-faint">
      保存时会用这个 Key 拉一次模型清单，拉不到就不存。模型清单之后自动更新，不用手填。
    </p>
  </div>
</Page>
