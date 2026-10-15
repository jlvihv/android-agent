<script lang="ts" module>
  /** `GET /api/providers` 的一行 */
  export type ProviderRow = {
    id: string;
    name: string;
    baseUrl: string;
    keyUrl: string | null;
    configured: boolean;
    /** Key 的最后 4 位（`…abcd`）；服务端从不回 Key 本身 */
    keyHint: string | null;
    /** 当前在用的就是这家 */
    current: boolean;
    /** 「其他（OpenAI 兼容）」：要自己填 Base URL */
    custom: boolean;
  };
</script>

<script lang="ts">
  /**
   * 设置页。从抽屉左下角的齿轮、或者选择模型面板里「模型提供商」页最下面的「管理模型提供商」进来。
   *
   * 几栏：「模型提供商」、「MCP 服务」（见 McpSection）、「技能（Skill）」（见 SkillsSection）、「连接手机」（配对码，见 PairingSection；只有电脑上自己开这个
   * 页面时才有）和「Linux 沙箱」（Alpine 版本 / 升级，见 SandboxSection；
   * 桌面宿主模式下没有这一栏）。换机器不在这里：在侧栏底栏的机器标签（见 MachineSheet）。模型提供商：内置几家服务商，每家只填 API Key；Base URL、
   * 协议都内置好了，模型清单自动拉（见 core/src/providers.rs）。几家可以
   * 同时配好，在选择模型面板里点哪家的模型就切到哪家。
   */
  import Icon from './Icon.svelte';
  import Page from './Page.svelte';
  import McpSection from './McpSection.svelte';
  import PairingSection from './PairingSection.svelte';
  import ProviderPage from './ProviderPage.svelte';
  import SandboxSection from './SandboxSection.svelte';
  import SkillsSection from './SkillsSection.svelte';
  import { store } from '../lib/store.svelte.ts';

  let { onclose }: { onclose: () => void } = $props();

  let rows = $state<ProviderRow[]>([]);
  let err = $state('');
  /** 正在看哪一家（叠在设置页上面的那一页） */
  let open = $state<ProviderRow | null>(null);

  async function load() {
    err = '';
    try {
      const r = await fetch('/api/providers');
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      rows = ((await r.json()) as { providers: ProviderRow[] }).providers;
    } catch (e) {
      err = String(e);
    }
  }

  $effect(() => {
    void load();
  });

  /** 详情页里存了 / 删了 Key：列表和输入卡片上的模型名都要跟着变 */
  async function changed() {
    await Promise.all([load(), store.reloadHealth()]);
  }

  function status(p: ProviderRow): string {
    if (!p.configured) return '未配置';
    return p.current ? `正在使用 · ${p.keyHint ?? ''}` : `已配置 · ${p.keyHint ?? ''}`;
  }
</script>

<Page title="设置" {onclose}>
  <h2 class="px-1 pt-2 pb-2 text-meta text-muted">模型提供商</h2>
  <div class="overflow-hidden rounded-2xl bg-surface">
    {#each rows as p (p.id)}
      <button
        onclick={() => (open = p)}
        class="flex w-full items-center gap-3 border-b-2 border-ink px-3.5 py-3 text-left
               last:border-b-0"
      >
        <span class="min-w-0 flex-1">
          <span class="block truncate text-[17px]">{p.name}</span>
          <span class="block truncate text-meta {p.current ? 'text-accent' : 'text-muted'}">
            {status(p)}
          </span>
        </span>
        <Icon name="chevron" size={16} class="shrink-0 text-muted" />
      </button>
    {/each}
  </div>
  {#if err}<p class="px-1 pt-3 text-xs text-bad">{err}</p>{/if}
  <McpSection />
  <SkillsSection />
  <PairingSection />
  <SandboxSection />
</Page>

{#if open}
  <ProviderPage provider={open} onclose={() => (open = null)} onchange={changed} />
{/if}
