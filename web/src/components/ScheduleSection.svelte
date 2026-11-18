<script lang="ts">
  /**
   * 设置页的「定时任务」一栏（见 core/src/schedule.rs）。
   *
   * 一条定时任务 = 「到点往 Router 的队列里投一句话，把它叫醒」。**它不指挥任何会话** ——
   * 派不派活、派给谁，是 Router 醒来之后的判断。所以这里写的是「要对协调层说什么」，
   * 不是「让谁干活」。
   *
   * `nextAt` **只从服务端来**，界面不自己算：两处算法迟早走偏（这个项目在
   * `standing` 上吃过一次亏，见 docs/16 §3.1）。
   */
  import Switch from "./Switch.svelte";
  import { api, post } from "../lib/api";

  type Schedule = {
    id: string;
    prompt: string;
    /** 给人看的周期（「每 30 分钟」/「cron `0 9 * * *`」） */
    every: string;
    everyMinutes: number | null;
    cron: string | null;
    paused: boolean;
    createdAt: number;
    /** 下一次触发（毫秒）；停用中是 null。**服务端现算的** */
    nextAt: number | null;
  };

  let list = $state<Schedule[]>([]);
  let max = $state(32);
  let promptMax = $state(2000);
  let tzOffset = $state(0);

  let prompt = $state("");
  let mode = $state<"every" | "cron">("every");
  let minutes = $state(30);
  let cron = $state("0 9 * * *");
  let busy = $state(false);
  let err = $state("");

  async function load() {
    try {
      const d = await api<{
        schedules: Schedule[];
        max: number;
        promptMaxChars: number;
        tzOffsetMinutes: number;
      }>("/api/schedules");
      list = d.schedules;
      max = d.max;
      promptMax = d.promptMaxChars;
      tzOffset = d.tzOffsetMinutes;
    } catch (e) {
      err = String(e);
    }
  }

  $effect(() => {
    void load();
  });

  async function add() {
    busy = true;
    err = "";
    try {
      await post("/api/schedules", {
        prompt: prompt.trim(),
        every_minutes: mode === "every" ? minutes : null,
        cron: mode === "cron" ? cron.trim() : null,
      });
      prompt = "";
      await load();
    } catch (e) {
      err = String(e).replace(/^Error: /, "");
    } finally {
      busy = false;
    }
  }

  async function setPaused(s: Schedule, paused: boolean) {
    err = "";
    try {
      await post("/api/schedules/paused", { id: s.id, paused });
      await load();
    } catch (e) {
      err = String(e).replace(/^Error: /, "");
    }
  }

  async function remove(s: Schedule) {
    err = "";
    try {
      await post("/api/schedules/delete", { id: s.id });
      await load();
    } catch (e) {
      err = String(e).replace(/^Error: /, "");
    }
  }

  /** 服务端给的是毫秒；界面显示成「今天 09:00」「明天 09:00」这种 */
  function when(at: number | null): string {
    if (at === null) return "已停用";
    const d = new Date(at);
    const now = new Date();
    const sameDay = d.toDateString() === now.toDateString();
    const time = d.toLocaleTimeString(undefined, {
      hour: "2-digit",
      minute: "2-digit",
    });
    if (sameDay) return `今天 ${time}`;
    const tomorrow = new Date(now.getTime() + 86_400_000);
    if (d.toDateString() === tomorrow.toDateString()) return `明天 ${time}`;
    return `${d.toLocaleDateString(undefined, { month: "numeric", day: "numeric" })} ${time}`;
  }

  const input =
    "min-w-0 flex-1 rounded-2xl bg-surface px-4 py-3.5 text-row outline-none placeholder:text-faint";
</script>

<h2 class="px-1 pt-6 pb-2 text-meta text-muted">定时任务</h2>

{#if list.length}
  <div class="overflow-hidden rounded-2xl bg-surface">
    {#each list as s (s.id)}
      <div
        class="flex items-center gap-3 border-b-2 border-ink px-3.5 py-3 last:border-b-0"
      >
        <span class="min-w-0 flex-1">
          <span
            class="line-clamp-2 text-row {s.paused ? 'text-muted' : ''}"
            >{s.prompt}</span
          >
          <span class="block text-meta text-faint">
            {s.every} · {s.paused ? "已停用" : when(s.nextAt)}
          </span>
        </span>
        <Switch
          checked={!s.paused}
          label="{s.paused ? '启用' : '停用'}「{s.prompt}」"
          onchange={(on) => setPaused(s, !on)}
        />
        <button
          onclick={() => remove(s)}
          class="shrink-0 text-meta text-bad"
          aria-label="删除「{s.prompt}」">删</button
        >
      </div>
    {/each}
  </div>
{/if}

{#if list.length < max}
  <div class="flex flex-col gap-2 pt-2">
    <input
      bind:value={prompt}
      placeholder="到点要对协调层说什么（比如「看看那台机器还活着没」）"
      maxlength={promptMax}
      class={input}
    />
    <div class="flex items-center gap-2">
      <button
        onclick={() => (mode = "every")}
        class="rounded-full px-3 py-1.5 text-meta {mode === 'every'
          ? 'bg-fg text-ink'
          : 'bg-surface text-muted'}">按间隔</button
      >
      <button
        onclick={() => (mode = "cron")}
        class="rounded-full px-3 py-1.5 text-meta {mode === 'cron'
          ? 'bg-fg text-ink'
          : 'bg-surface text-muted'}">按钟点</button
      >
      {#if mode === "every"}
        <span class="text-meta text-muted">每</span>
        <input
          bind:value={minutes}
          type="number"
          min="1"
          max="43200"
          class="w-20 rounded-2xl bg-surface px-3 py-1.5 text-row outline-none"
        />
        <span class="text-meta text-muted">分钟</span>
      {:else}
        <input
          bind:value={cron}
          placeholder="0 9 * * *"
          autocapitalize="off"
          autocorrect="off"
          spellcheck="false"
          class="min-w-0 flex-1 rounded-2xl bg-surface px-3 py-1.5 text-row outline-none placeholder:text-faint"
        />
      {/if}
    </div>
    <button
      onclick={add}
      disabled={busy || !prompt.trim()}
      class="rounded-full bg-fg py-3 text-ui font-medium text-ink disabled:opacity-40"
    >
      加上
    </button>
  </div>
{/if}

<p class="px-1 pt-2 text-meta leading-relaxed text-faint">
  到点只做一件事：往协调层投一句话，把它叫醒。派不派活是它的判断。<br />
  按钟点的时间按**现在**的时区算（此刻 UTC{tzOffset >= 0 ? "+" : ""}{tzOffset /
    60}）—— 你换了时区，钟点跟着现在的时区走。
</p>
{#if list.length >= max}
  <p class="px-1 pt-2 text-meta text-warn">
    已经有 {max} 条了（上限）。删掉几条再加。
  </p>
{/if}
{#if err}<p class="px-1 pt-2 text-xs text-bad">{err}</p>{/if}
