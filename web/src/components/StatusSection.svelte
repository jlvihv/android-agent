<script lang="ts">
  /**
   * 设置页最底下「状态」一栏：各部件现在通不通（`/api/health`），以及版本。
   *
   * 不正常的那行标出来；在手机 App 里、毛病又是缺权限的（无障碍没开），直接给一个
   * 「去开启」（同一个 `AgentHost.grant`，见 PermissionsSection）。以前要靠电脑上跑
   * `just doctor` 才看得到这些。
   */
  type Health = {
    version: string;
    git?: string;
    platform?: string;
    computer?: string | null;
    llm?: { name: string; model: string } | null;
    linux?: { host: boolean; workspace?: string | null } | null;
    device?: string | null;
    browser?: string | null;
  };
  type Row = { label: string; value: string; bad?: boolean; grant?: string };

  const host = (globalThis as { AgentHost?: { grant?: (k: string) => void } }).AgentHost;

  let h = $state<Health | null>(null);

  async function load() {
    try {
      const r = await fetch('/api/health');
      if (r.ok) h = (await r.json()) as Health;
    } catch {
      /* 连不上核心：整栏不显示 */
    }
  }

  $effect(() => {
    void load();
    const onVisible = () => document.visibilityState === 'visible' && load();
    document.addEventListener('visibilitychange', onVisible);
    return () => document.removeEventListener('visibilitychange', onVisible);
  });

  const PLATFORM: Record<string, string> = { android: '安卓', macos: 'macOS', linux: 'Linux' };

  const rows = $derived.by((): Row[] => {
    if (!h) return [];
    const phone = h.platform === 'android';
    const out: Row[] = [];
    out.push(
      h.llm?.model
        ? { label: '模型', value: `${h.llm.name} / ${h.llm.model}` }
        : { label: '模型', value: '还没配（上面「模型提供商」里填 Key）', bad: true },
    );
    out.push(
      h.linux
        ? {
            label: h.linux.host ? '命令在这台电脑上跑' : 'Linux 沙箱',
            value: h.linux.workspace ? `工作目录 ${h.linux.workspace}` : '已就绪',
          }
        : { label: 'Linux 沙箱', value: '还没起来（首次启动要解压，稍等）', bad: true },
    );
    if (phone) {
      out.push(
        h.device === 'accessibility'
          ? { label: '设备操作', value: '无障碍已连上' }
          : { label: '设备操作', value: '无障碍没开：点按、滑动、按键、截屏都用不了', bad: true, grant: 'accessibility' },
      );
    }
    out.push(
      h.browser
        ? { label: '浏览器', value: h.browser }
        : { label: '浏览器', value: '不可用', bad: true },
    );
    const where = [PLATFORM[h.platform ?? ''] ?? h.platform, h.computer].filter(Boolean).join(' · ');
    out.push({ label: '版本', value: `${h.version}${h.git ? `（${h.git}）` : ''}${where ? ` · ${where}` : ''}` });
    return out;
  });
</script>

{#if rows.length}
  <h2 class="px-1 pt-6 pb-2 text-meta text-muted">状态</h2>
  <div class="overflow-hidden rounded-2xl bg-surface">
    {#each rows as r (r.label)}
      <div class="flex items-center gap-3 border-b-2 border-ink px-3.5 py-3 last:border-b-0">
        <span class="min-w-0 flex-1">
          <span class="block text-[15px]">{r.label}</span>
          <span class="block text-meta break-all {r.bad ? 'text-bad' : 'text-muted'}">{r.value}</span>
        </span>
        {#if r.grant && host?.grant}
          <button onclick={() => host?.grant?.(r.grant!)} class="shrink-0 text-meta text-accent">去开启</button>
        {/if}
      </div>
    {/each}
  </div>
{/if}
