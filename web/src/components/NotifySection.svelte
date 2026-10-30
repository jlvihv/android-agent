<script lang="ts">
  /**
   * 设置页「通知」一栏：回合跑完时弹不弹**系统通知**。
   *
   * 发通知的是核心，不是浏览器（浏览器那条路走不通：`Notification` 只在安全上下文里
   * 存在，`http://192.168.x.x` 那种地址浏览器连权限都不给）。所以这里只读写核心的
   * 配置（`/api/notify`），跟「这个浏览器」无关 —— 发通知的是你面前那台电脑。
   *
   * 手机上整栏不显示：那边是壳的前台服务在发（`AgentNotifications.showDone`），
   * 核心再发一次就是同一个任务响两下。
   */
  import Switch from "./Switch.svelte";
  import { store } from "../lib/store.svelte.ts";

  type Config = {
    /** 这个平台会不会发系统通知（手机上恒 false） */
    available: boolean;
    enabled: boolean;
    /** 不会发的时候为什么（设置页照抄它） */
    reason: string | null;
  };

  let cfg = $state<Config | null>(null);
  let busy = $state(false);
  let err = $state("");

  const phone = $derived(store.platform === "android");

  async function load() {
    try {
      const r = await fetch("/api/notify");
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      cfg = (await r.json()) as Config;
    } catch (e) {
      err = String(e).replace(/^Error: /, "");
    }
  }

  $effect(() => {
    // 手机上整栏不显示，也就没必要问
    if (!phone) void load();
  });

  async function toggle(next: boolean) {
    busy = true;
    err = "";
    try {
      const r = await fetch("/api/notify", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ enabled: next }),
      });
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      await load();
    } catch (e) {
      err = String(e).replace(/^Error: /, "");
    } finally {
      busy = false;
    }
  }
</script>

{#if !phone && cfg}
  <h2 class="px-1 pt-6 pb-2 text-meta text-muted">通知</h2>
  <div class="overflow-hidden rounded-2xl bg-surface">
    <div
      class="flex items-center gap-3 border-b-2 border-ink px-3.5 py-3 last:border-b-0"
    >
      <span class="min-w-0 flex-1">
        <span class="block truncate text-row">任务完成时通知我</span>
        <span class="block truncate text-meta text-muted">
          {#if cfg.reason}
            {cfg.reason}
          {:else if cfg.enabled}
            跑完弹一条系统通知；你正看着这个页面时也会弹
          {:else}
            已关闭：跑完不弹通知
          {/if}
        </span>
      </span>
      <Switch
        checked={cfg.enabled}
        label="任务完成时通知我"
        onchange={toggle}
      />
    </div>
  </div>
  <p class="px-1 pt-2 text-meta leading-relaxed text-faint">
    通知由**你面前这台电脑的核心**发：在 Mac
    的浏览器里连着另一台机器时，跑完会在 Mac
    上提醒你（那台机器跑完了，本机去通知你）。页关着也照样能收到。
  </p>
  {#if err}<p class="px-1 pt-2 text-xs text-bad">{err}</p>{/if}
{/if}
