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
   *
   * ## 编辑
   *
   * 每行的「改」把那一行换成编辑表单（和下面「加一条」是同一套控件，只是带着
   * 原来的值）。**提交时只发改过的那几样**：
   *
   * - 只改文字 → 只发 `prompt`，服务端不动时间（8:59 改错别字，9:00 照常响）
   * - 只改时间 → 只发 `every_minutes` / `cron`，服务端把节奏从「现在」重算
   * - 两样都改 → 两样都发
   *
   * 「只发改过的」这一步很重要：把没改的那半也发过去，服务端会把它当成「同一份
   * 不变的值」（它自己有 `t.every != e` 那层比较兜着），但界面这边没必要赌。
   *
   * 编辑**不改暂停状态**：改一条停用的任务，改完还是停用（服务端保证，界面也只是
   * 把表单收起来）。
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
    /** **这一版节奏的起点**（改节奏会重置它），不是「创建时刻」 */
    anchorAt: number;
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

  /** 正在编辑哪一条（`null` = 没有）。同一时刻只编辑一条 —— 手机上两张表单叠着
   *  没法用，而且「改了哪条」这件事只该有一个答案。 */
  let editingId = $state<string | null>(null);
  /** 编辑态的那几个字段（和「加一条」分开，取消时不能把新任务的草稿冲掉） */
  let editPrompt = $state("");
  let editMode = $state<"every" | "cron">("every");
  let editMinutes = $state(30);
  let editCron = $state("0 9 * * *");

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

  /** 进入编辑态：把那一条的**当前值**填进表单。 */
  function startEdit(s: Schedule) {
    editingId = s.id;
    editPrompt = s.prompt;
    if (s.cron !== null) {
      editMode = "cron";
      editCron = s.cron;
    } else {
      editMode = "every";
      editMinutes = s.everyMinutes ?? 30;
    }
    err = "";
  }

  function cancelEdit() {
    editingId = null;
    err = "";
  }

  /**
   * 提交编辑。**只发改过的字段**（理由见文件头）。
   *
   * 一个都没改就不发请求 —— 省一次往返，也避免服务端那条 `t.every != e` 的比较
   * 被「同一份值」白白走一遍（虽然它不会出问题）。
   */
  async function submitEdit(s: Schedule) {
    const body: Record<string, unknown> = { id: s.id };
    const p = editPrompt.trim();
    if (p && p !== s.prompt) body.prompt = p;
    // 节奏：只有「模式变了」或者「值变了」才算改
    const wantCron = editMode === "cron" ? editCron.trim() : null;
    const wantMinutes = editMode === "every" ? editMinutes : null;
    if (wantCron !== null) {
      if (wantCron !== s.cron) body.cron = wantCron;
    } else if (wantMinutes !== s.everyMinutes) {
      body.every_minutes = wantMinutes;
    }
    if (Object.keys(body).length === 1) {
      // 只带了 id：什么都没改
      editingId = null;
      return;
    }
    busy = true;
    err = "";
    try {
      await post("/api/schedules/edit", body);
      editingId = null;
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
  const num = "w-20 rounded-2xl bg-surface px-3 py-1.5 text-row outline-none";
  const pill = "rounded-full px-3 py-1.5 text-meta";
</script>

<h2 class="px-1 pt-6 pb-2 text-meta text-muted">定时任务</h2>

{#if list.length}
  <div class="overflow-hidden rounded-2xl bg-surface">
    {#each list as s (s.id)}
      {#if editingId === s.id}
        <!-- 编辑态：整行换成表单（不新开浮层 —— 手机上多一层就多一次点错） -->
        <div
          class="flex flex-col gap-2 border-b-2 border-ink px-3.5 py-3 last:border-b-0"
        >
          <input
            bind:value={editPrompt}
            placeholder="到点要对协调层说什么"
            maxlength={promptMax}
            class={input}
          />
          <div class="flex items-center gap-2">
            <button
              onclick={() => (editMode = "every")}
              class="{pill} {editMode === 'every'
                ? 'bg-fg text-ink'
                : 'bg-surface text-muted'}">按间隔</button
            >
            <button
              onclick={() => (editMode = "cron")}
              class="{pill} {editMode === 'cron'
                ? 'bg-fg text-ink'
                : 'bg-surface text-muted'}">按钟点</button
            >
            {#if editMode === "every"}
              <span class="text-meta text-muted">每</span>
              <input
                bind:value={editMinutes}
                type="number"
                min="1"
                max="43200"
                class={num}
              />
              <span class="text-meta text-muted">分钟</span>
            {:else}
              <input
                bind:value={editCron}
                placeholder="0 9 * * *"
                autocapitalize="off"
                autocorrect="off"
                spellcheck="false"
                class="{input} px-3 py-1.5"
              />
            {/if}
          </div>
          <div class="flex gap-2">
            <button
              onclick={() => submitEdit(s)}
              disabled={busy || !editPrompt.trim()}
              class="flex-1 rounded-full bg-fg py-2.5 text-ui font-medium text-ink disabled:opacity-40"
            >
              保存
            </button>
            <button
              onclick={cancelEdit}
              disabled={busy}
              class="rounded-full px-4 py-2.5 text-ui text-muted"
            >
              取消
            </button>
          </div>
          {#if s.paused}
            <p class="text-meta text-faint">
              这条现在停用着 —— 改完还是停用，要它开始响得把开关打开。
            </p>
          {/if}
        </div>
      {:else}
        <div
          class="flex items-center gap-3 border-b-2 border-ink px-3.5 py-3 last:border-b-0"
        >
          <span class="min-w-0 flex-1">
            <span class="line-clamp-2 text-row {s.paused ? 'text-muted' : ''}"
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
            onclick={() => startEdit(s)}
            class="shrink-0 text-meta text-muted"
            aria-label="改「{s.prompt}」">改</button
          >
          <button
            onclick={() => remove(s)}
            class="shrink-0 text-meta text-bad"
            aria-label="删除「{s.prompt}」">删</button
          >
        </div>
      {/if}
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
        class="{pill} {mode === 'every'
          ? 'bg-fg text-ink'
          : 'bg-surface text-muted'}">按间隔</button
      >
      <button
        onclick={() => (mode = "cron")}
        class="{pill} {mode === 'cron'
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
          class={num}
        />
        <span class="text-meta text-muted">分钟</span>
      {:else}
        <input
          bind:value={cron}
          placeholder="0 9 * * *"
          autocapitalize="off"
          autocorrect="off"
          spellcheck="false"
          class="{input} px-3 py-1.5"
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
    60}）—— 你换了时区，钟点跟着现在的时区走。<br />
  改时间 = 从改完那一刻起按新节奏重算（不会立刻响一次）。
</p>
{#if list.length >= max}
  <p class="px-1 pt-2 text-meta text-warn">
    已经有 {max} 条了（上限）。删掉几条再加。
  </p>
{/if}
{#if err}<p class="px-1 pt-2 text-xs text-bad">{err}</p>{/if}
