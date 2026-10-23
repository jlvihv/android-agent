<script lang="ts">
  /**
   * 设置页里的「连接手机」：电脑上跑宿主核心时，手机 App 的「连接电脑」要输的那个
   * 6 位配对码（`desktop::Pairing`）。
   *
   * 这个码只在核心启动时往终端打一行的话，后台起的核心
   * （`setsid`、日志重定向进文件、或者在别的终端窗口里）那行就找不着了 ——
   * 打开设置页就该看得见。
   *
   * **只有这台电脑自己开这个页面时才有**：`/api/pairing` 只回给 loopback
   * （码的意义就是「证明你人在电脑前」）。手机上、核心没对局域网开放时
   * 都是 `available: false`，整栏不显示。
   */
  type Info = {
    available: boolean;
    code?: string;
    /** 还能输错几次；`0` = 已经停用（重启核心换新码） */
    failsLeft?: number;
    name?: string;
  };

  let st = $state<Info | null>(null);

  $effect(() => {
    void (async () => {
      try {
        const r = await fetch("/api/pairing");
        // 老核心没有这个接口（404）：整栏不显示，这不是错误
        if (r.ok) st = (await r.json()) as Info;
      } catch {
        // 拿不到就当没有 —— 设置页别的地方会把真错误显示出来
      }
    })();
  });

  /** 输错太多次，这个码废了：显示状态，不再显示码（显示也没用） */
  const locked = $derived(st?.available === true && st.failsLeft === 0);
</script>

{#if st?.available}
  <h2 class="px-1 pt-6 pb-2 text-meta text-muted">连接手机</h2>
  <div class="overflow-hidden rounded-2xl bg-surface">
    <div class="flex items-center gap-3 px-3.5 py-3">
      <span class="min-w-0 flex-1">
        <span class="block text-[17px]">配对码</span>
        <span
          class="block truncate text-meta {locked ? 'text-bad' : 'text-muted'}"
        >
          {locked ? "已停用" : "手机第一次连这台电脑时输一次"}
        </span>
      </span>
      {#if locked}
        <span class="shrink-0 py-1 text-[15px] text-bad">重启核心换新码</span>
      {:else}
        <span
          class="shrink-0 font-mono text-[26px] leading-none tracking-[0.14em] text-fg"
        >
          {st.code}
        </span>
      {/if}
    </div>
  </div>
  <p class="px-1 pt-3 text-meta leading-relaxed text-faint">
    手机 App → 设置 → 连接电脑 → 点「{st.name}」→ 输这个码。手机和电脑要在同一个
    Wi-Fi 里。 输错几次这个码就停用，重启电脑上的核心会换一个。
  </p>
{/if}
