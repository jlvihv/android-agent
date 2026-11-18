<script lang="ts">
  /**
   * 协调层那条会话顶上的「在干的活」。
   *
   * 为什么要有它：Router 派出去的活跑在**别的**会话里，用户在当前这条看不到任何
   * 痕迹 —— 界面会说「说点什么」而实际上有三件事正在跑。这块就是把工作项摊开：
   * 各自到哪一步、谁在等人。
   *
   * **只读**。点一行不做任何事（将来可以是「跳过去看那条会话」，但那需要先想清
   * 「跳过去之后 Router 的上下文怎么办」，先不做）。
   *
   * 数据来自 `/api/tickets`，跟着转发回合和定时轮询刷新（见 App 的 `refreshTickets`），
   * 不在这里自己开定时器 —— 那样两个 Router 会话各开一个，越切越多。
   */
  import Icon from "./Icon.svelte";

  /** 一个工作项（`/api/tickets` 的 `tickets[]`）。 */
  export type TicketRow = {
    id: string;
    number: number;
    title: string;
    /** 机器读的状态（`working` / `waiting_review` …） */
    stage: string;
    /** 人看的状态名（「在干」「待验收」…） */
    label: string;
    /** 要不要人来处理（`person/*` 那几个） */
    needsPerson: boolean;
    open: boolean;
    report?: string | null;
    /** 「还在动」（最近一次汇报是 progress，**而且**状态真的还在跑） */
    moving?: boolean;
    preview?: string;
    dispatches?: number;
  };

  let {
    tickets = [],
    working = 0,
    waiting = 0,
  }: {
    tickets?: TicketRow[];
    working?: number;
    waiting?: number;
  } = $props();

  /**
   * 状态 → 那个小圆点的颜色。
   *
   * 「等人」的一律用 warn（那是要你动的），「在干」用 muted（不用你管），
   * 结束的用更淡的。**不按 stage 一个个配颜色** —— 七种颜色在一行小字里
   * 分不出来，真正要紧的只有「要不要我管」这一个二分。
   */
  const dot = (t: TicketRow): string =>
    t.needsPerson ? "bg-warn" : t.open ? "bg-muted" : "bg-faint";

  /** 抬头那一句。一件事都没有时整块不显示（别占地方）。 */
  const summary = $derived(
    waiting > 0 ? `${working} 件在干，${waiting} 件等你` : `${working} 件在干`,
  );
</script>

{#if tickets.length > 0}
  <!-- 顶部留白：header 是浮层（`absolute top-0`），内容从它**下面**穿过去。
       不留这一截的话，抬头那句「N 件在干」会被菜单图标压住（踩过）。
       用和消息列表第一行同样的量：`--fade-top` 那一段 + header 自己的高度。 -->
  <!-- 高度**必须有上限**：`main` 里它和 MessageList 是兄弟，而 MessageList 是
       `h-full` —— 面板没上限的话两者加起来超过 100%，底部就被输入框盖住了
       （线上抓到的 bug）。`max-h` 之后列表还拿 `h-full`，但多出来的部分会
       被 `main` 裁掉，输入框的 `--composer-h` 留白才真的够用。 -->
  <div class="px-4 pb-1 pt-[calc(2.75rem+env(safe-area-inset-top))]">
    <div
      class="max-h-[38vh] overflow-y-auto overscroll-y-contain rounded-2xl bg-surface px-3 py-2 text-sm"
      aria-label="在干的活"
    >
      <p class="mb-1 flex items-center gap-1.5 text-xs text-faint">
        <Icon name="sparkle" size={12} class="shrink-0 text-muted" />
        <span>{summary}</span>
      </p>
      <ul class="space-y-1">
        {#each tickets as t (t.id)}
          <li class="flex items-start gap-2 leading-snug">
            <span
              class="mt-1.5 size-1.5 shrink-0 rounded-full {dot(t)}"
              aria-hidden="true"
            ></span>
            <span class="min-w-0 flex-1">
              <span class="text-fg">{t.title}</span>
              <span class="ml-1.5 text-xs text-faint">{t.label}</span>
              {#if t.preview}
                <span class="block truncate text-xs text-muted"
                  >{t.preview}</span
                >
              {/if}
            </span>
          </li>
        {/each}
      </ul>
    </div>
  </div>
{/if}
