<script lang="ts">
  /**
   * 设置页「手机权限」一栏：无障碍、所有文件、通知、电池优化、通知读取、通讯录……
   * 每项现在开没开，没开的点一下去开（能弹框的弹框，不能的送去系统设置页）。
   *
   * 状态和动作都在壳里（`PermissionCenter.kt`，经 `AgentHost` 桥）—— 所以只有在
   * 手机 App 里才有这一栏，电脑上的浏览器没有 `AgentHost`，整栏不显示。
   *
   * 从系统设置回来、弹框点完，状态是壳那边变的，页面收不到通知：回到前台时重新问一次，
   * 点了「去开启」之后再短轮询一阵（弹框不会让页面失焦）。
   */
  import Icon from "./Icon.svelte";

  type Item = {
    key: string;
    title: string;
    desc: string;
    state: "granted" | "missing" | "info";
  };
  type Host = { permissions?: () => string; grant?: (key: string) => void };

  const host = (globalThis as { AgentHost?: Host }).AgentHost;
  const available = typeof host?.permissions === "function";

  let items = $state<Item[]>([]);

  function load() {
    if (!available) return;
    try {
      items = JSON.parse(host!.permissions!()) as Item[];
    } catch {
      /* 壳回了看不懂的东西：保持原样 */
    }
  }

  /** 点了「去开启」之后轮询多久（弹框点完 / 从设置页回来都在这段里） */
  const POLL_MS = 1000;
  const POLL_FOR_MS = 30_000;
  let poll: ReturnType<typeof setInterval> | null = null;

  function grant(key: string) {
    host?.grant?.(key);
    if (poll) clearInterval(poll);
    const until = Date.now() + POLL_FOR_MS;
    poll = setInterval(() => {
      load();
      if (Date.now() > until && poll) {
        clearInterval(poll);
        poll = null;
      }
    }, POLL_MS);
  }

  $effect(() => {
    load();
    const onVisible = () => document.visibilityState === "visible" && load();
    document.addEventListener("visibilitychange", onVisible);
    window.addEventListener("focus", load);
    return () => {
      document.removeEventListener("visibilitychange", onVisible);
      window.removeEventListener("focus", load);
      if (poll) clearInterval(poll);
    };
  });

  const missing = $derived(items.filter((i) => i.state === "missing").length);
</script>

{#if available && items.length}
  <h2 class="px-1 pt-6 pb-2 text-meta text-muted">
    手机权限{missing ? ` · ${missing} 项没开` : ""}
  </h2>
  <div class="overflow-hidden rounded-2xl bg-surface">
    {#each items as it (it.key)}
      {#if it.state === "missing"}
        <button
          onclick={() => grant(it.key)}
          class="flex w-full items-center gap-3 border-b-2 border-ink px-3.5 py-3 text-left last:border-b-0"
        >
          <span class="min-w-0 flex-1">
            <span class="block text-[17px]">{it.title}</span>
            <span class="block text-meta text-muted">{it.desc}</span>
          </span>
          <span class="shrink-0 text-meta text-accent">去开启</span>
          <Icon name="chevron" size={16} class="shrink-0 text-muted" />
        </button>
      {:else}
        <div
          class="flex items-center gap-3 border-b-2 border-ink px-3.5 py-3 last:border-b-0"
        >
          <span class="min-w-0 flex-1">
            <span class="block text-[17px]">{it.title}</span>
            <span class="block text-meta text-muted">{it.desc}</span>
          </span>
          {#if it.state === "granted"}
            <span class="shrink-0 text-meta text-faint">已开启</span>
          {/if}
        </div>
      {/if}
    {/each}
  </div>
{/if}
