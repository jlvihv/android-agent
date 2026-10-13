<script lang="ts">
  /**
   * 设置页里的「Linux 沙箱」一栏：Alpine 版本、检查更新、一键升级。
   *
   * 升级本身在核心里（core/src/alpine.rs）：先在沙箱外把整个 rootfs 备份一份，
   * 改软件源 + `apk upgrade --available`，验收不过就换回备份。这里只负责发起、
   * 轮询进度、把 apk 的输出尾巴给人看。
   *
   * 宿主模式（桌面）没有 Alpine，`available: false`，整栏不显示。
   */
  type Job = {
    state: 'idle' | 'running' | 'done' | 'failed';
    from: string;
    to: string;
    step: string;
    log: string[];
    error: string | null;
  };
  type Status = {
    available: boolean;
    current?: string | null;
    latest?: string | null;
    upgradable?: boolean;
    checkError?: string | null;
    job: Job;
  };

  let st = $state<Status | null>(null);
  let err = $state('');
  let checking = $state(false);
  /** 点了一次「升级」：再点一次才真的开始（这是个几分钟、几百 MB 的操作） */
  let confirming = $state(false);
  let starting = $state(false);

  const running = $derived(st?.job.state === 'running');

  async function load(check = false) {
    try {
      const r = await fetch(`/api/alpine${check ? '?check=1' : ''}`);
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      st = (await r.json()) as Status;
      err = '';
    } catch (e) {
      err = String(e);
    }
  }

  async function check() {
    checking = true;
    await load(true);
    checking = false;
  }

  async function upgrade() {
    if (!confirming) {
      confirming = true;
      return;
    }
    confirming = false;
    starting = true;
    try {
      const r = await fetch('/api/alpine/upgrade', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(st?.latest ? { to: st.latest } : {}),
      });
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      await load();
    } catch (e) {
      err = String(e);
    }
    starting = false;
  }

  // 打开设置就顺手查一次最新版（一个很小的 yaml）
  $effect(() => {
    void load(true);
  });

  // 升级中每 1.5 秒拉一次进度
  $effect(() => {
    if (!running) return;
    const t = setInterval(() => void load(), 1500);
    return () => clearInterval(t);
  });
</script>

{#if st?.available}
  <h2 class="px-1 pt-6 pb-2 text-meta text-muted">Linux 沙箱</h2>
  <div class="overflow-hidden rounded-2xl bg-surface">
    <div class="flex items-center gap-3 px-3.5 py-3">
      <span class="min-w-0 flex-1">
        <span class="block text-[17px]">Alpine {st.current ?? '?'}</span>
        <span class="block truncate text-meta {st.upgradable ? 'text-accent' : 'text-muted'}">
          {#if running}
            正在升级到 {st.job.to}…
          {:else if st.upgradable}
            有新版本 {st.latest}
          {:else if st.latest}
            已是最新
          {:else if st.checkError}
            查不到最新版本
          {:else}
            —
          {/if}
        </span>
      </span>
      {#if !running}
        <button
          onclick={check}
          disabled={checking}
          class="shrink-0 py-1 text-[15px] text-muted disabled:opacity-40"
        >
          {checking ? '检查中…' : '检查更新'}
        </button>
      {/if}
    </div>

    {#if st.upgradable && !running}
      <div class="border-t-2 border-ink px-3.5 py-3">
        <button
          onclick={upgrade}
          disabled={starting}
          class="w-full rounded-full bg-fg py-3 text-[15px] font-medium text-ink disabled:opacity-40"
        >
          {confirming ? `确认升级到 ${st.latest}？` : `升级到 ${st.latest}`}
        </button>
        {#if confirming}
          <button onclick={() => (confirming = false)} class="w-full pt-2.5 text-[15px] text-muted">
            取消
          </button>
        {/if}
      </div>
    {/if}

    {#if st.job.state !== 'idle'}
      <div class="border-t-2 border-ink px-3.5 py-3">
        <p
          class="text-meta {st.job.state === 'failed'
            ? 'text-bad'
            : st.job.state === 'done'
              ? 'text-accent'
              : 'text-muted'}"
        >
          {#if st.job.state === 'failed'}
            升级失败：{st.job.error}
          {:else if st.job.state === 'done'}
            已升级：{st.job.from} → {st.job.to}
          {:else}
            {st.job.step}
          {/if}
        </p>
        {#if st.job.log.length}
          <pre
            class="mt-2 max-h-48 overflow-y-auto font-mono text-[11px] leading-4 whitespace-pre-wrap break-all text-faint">{st.job.log
              .slice(-12)
              .join('\n')}</pre>
        {/if}
      </div>
    {/if}
  </div>
  {#if err}<p class="px-1 pt-3 text-xs text-bad">{err}</p>{/if}
  <p class="px-1 pt-3 text-meta leading-relaxed text-faint">
    原地升级：装过的包、/root 里的东西都留着。会先把整个沙箱备份一份，升级不成功就自动换回。
    要下载几百 MB、用几分钟，期间不能跑命令 —— 请留在 App 里。
  </p>
{/if}
