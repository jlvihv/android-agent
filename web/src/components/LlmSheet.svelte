<script lang="ts">
  /** 模型设置。从输入卡片上的模型药丸打开。 */
  import { store } from '../lib/store.svelte.ts';
  import Sheet from './Sheet.svelte';

  let { onclose }: { onclose: () => void } = $props();

  const presets = [
    { label: 'OpenCode Go', url: 'https://opencode.ai/zen/go/v1', model: 'deepseek-v4.1-flash' },
    { label: 'OpenAI', url: 'https://api.openai.com/v1', model: 'gpt-5' },
    { label: 'DeepSeek', url: 'https://api.deepseek.com/v1', model: 'deepseek-chat' },
    { label: 'Ollama', url: 'http://127.0.0.1:11434/v1', model: 'qwen3' },
  ];

  let baseUrl = $state(store.llm?.baseUrl ?? presets[0].url);
  let model = $state(store.llm?.model ?? presets[0].model);
  let apiKey = $state('');
  let saving = $state(false);
  let err = $state('');

  async function save() {
    err = '';
    if (!model.trim()) return (err = 'model 不能为空');
    if (!apiKey.trim() && !store.llm) return (err = 'API key 不能为空');
    saving = true;
    try {
      const r = await fetch('/api/llm', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          name: 'custom',
          baseUrl: baseUrl.trim(),
          model: model.trim(),
          apiKey: apiKey.trim() || '(unchanged)',
        }),
      });
      if (!r.ok) throw new Error(await r.text());
      await store.reloadHealth();
      onclose();
    } catch (e) {
      err = String(e);
    } finally {
      saving = false;
    }
  }
</script>

<Sheet title="模型设置" onclose={onclose}>
  <div class="flex flex-col gap-3">
    <div class="flex gap-1.5 overflow-x-auto">
    {#each presets as p (p.label)}
      <button
        onclick={() => {
          baseUrl = p.url;
          model = p.model;
        }}
        class="shrink-0 rounded-full bg-hairline px-3 py-1.5 text-xs whitespace-nowrap"
      >
        {p.label}
      </button>
    {/each}
  </div>

  {#each [['Base URL', baseUrl, 'https://…/v1', 'text'], ['模型', model, 'deepseek-v4.1-flash', 'text'], ['API Key', apiKey, store.llm ? '留空则不修改' : 'sk-…', 'password']] as const as [label, val, ph, type] (label)}
    <label class="flex flex-col gap-1">
      <span class="text-meta text-muted">{label}</span>
      <input
        type={type}
        value={val}
        placeholder={ph}
        autocapitalize="off"
        autocorrect="off"
        oninput={(e) => {
          const v = e.currentTarget.value;
          if (label === 'Base URL') baseUrl = v;
          else if (label === '模型') model = v;
          else apiKey = v;
        }}
        class="rounded-xl border border-control bg-surface px-3.5 py-3 text-sm outline-none
               placeholder:text-faint"
      />
    </label>
  {/each}

  {#if err}<p class="text-xs text-bad">{err}</p>{/if}

  <button
    onclick={save}
    disabled={saving}
    class="rounded-full bg-fg py-3 text-sm font-medium text-ink disabled:opacity-50"
  >
    {saving ? '保存中…' : '保存'}
  </button>

  <p class="text-meta leading-snug text-faint">
    配置存在 App 私有目录 <code class="font-mono">llm.json</code>，目前是明文（受 Android 沙箱保护）。
  </p>
  </div>
</Sheet>
