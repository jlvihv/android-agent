<script lang="ts">
  /**
   * 协调层那条会话顶上的「我派出去的会话」。
   *
   * 为什么要有它：Router 派出去的活跑在**别的**会话里，用户在当前这条看不到任何
   * 痕迹 —— 界面会说「说点什么」而实际上有三件事正在跑。这块就是把它们摊开：
   * 各自在跑还是闲着、谁在等人。
   *
   * **只读**。点一行不做任何事（将来可以是「跳过去看那条会话」，但那需要先想清
   * 「跳过去之后 Router 的上下文怎么办」，先不做）。
   *
   * 数据来自 `/api/sessions/dispatched`，跟着转发回合和定时轮询刷新
   * （见 App 的 `refreshDispatched`），不在这里自己开定时器 —— 那样两个 Router
   * 会话各开一个，越切越多。
   */
  import Icon from "./Icon.svelte";

  /** 一条派出去的会话（`/api/sessions/dispatched` 的 `sessions[]`）。 */
  export type DispatchedRow = {
    id: string;
    title: string;
    /** 机器读的现算状态：`working` / `idle` / `done` / `cancelled` */
    state: string;
    /** 人看的状态名（「在跑」「闲着」「已验收」「已取消」） */
    label: string;
    /** 要不要人来处理（停了、还没收尾） */
    needsPerson: boolean;
    /** 还没收尾 */
    open: boolean;
    /** 回合真的在跑 */
    busy: boolean;
    /** 最新结论（已截到 200 字） */
    conclusion?: string;
    dispatches?: number;
  };

  let {
    sessions = [],
    working = 0,
    waiting = 0,
  }: {
    sessions?: DispatchedRow[];
    working?: number;
    waiting?: number;
  } = $props();

  /**
   * 状态 → 那个小圆点的颜色。
   *
   * 「等人」的一律用 warn（那是要你动的），「在跑」用 muted（不用你管），
   * 结束的用更淡的。**不按 state 一个个配颜色** —— 四种颜色在一行小字里
   * 分不出来，真正要紧的只有「要不要我管」这一个二分。
   */
  const dot = (s: DispatchedRow): string =>
    s.needsPerson ? "bg-warn" : s.open ? "bg-muted" : "bg-faint";

  /** 抬头那一句。一条都没有时整块不显示（别占地方）。 */
  const summary = $derived(
    waiting > 0 ? `${working} 个在跑，${waiting} 个等你` : `${working} 个在跑`,
  );
</script>

{#if sessions.length > 0}
  <!-- 顶部留白：header 是浮层（`absolute top-0`），内容从它**下面**穿过去。
       不留这一截的话，抬头那句「N 个在跑」会被菜单图标压住（踩过）。
       用和消息列表第一行同样的量：`--fade-top` 那一段 + header 自己的高度。 -->
  <!-- 高度**必须有上限**：`main` 里它和 MessageList 是兄弟，而 MessageList 是
       `h-full` —— 面板没上限的话两者加起来超过 100%，底部就被输入框盖住了
       （线上抓到的 bug）。`max-h` 之后列表还拿 `h-full`，但多出来的部分会
       被 `main` 裁掉，输入框的 `--composer-h` 留白才真的够用。 -->
  <div class="px-4 pb-1 pt-[calc(2.75rem+env(safe-area-inset-top))]">
    <div
      class="max-h-[38vh] overflow-y-auto overscroll-y-contain rounded-2xl bg-surface px-3 py-2 text-sm"
      aria-label="派出去的会话"
    >
      <p class="mb-1 flex items-center gap-1.5 text-xs text-faint">
        <Icon name="sparkle" size={12} class="shrink-0 text-muted" />
        <span>{summary}</span>
      </p>
      <ul class="space-y-1">
        {#each sessions as t (t.id)}
          <li class="flex items-start gap-2 leading-snug">
            <span
              class="mt-1.5 size-1.5 shrink-0 rounded-full {dot(t)}"
              aria-hidden="true"
            ></span>
            <span class="min-w-0 flex-1">
              <span class="text-fg">{t.title}</span>
              <span class="ml-1.5 text-xs text-faint">{t.label}</span>
              {#if t.conclusion}
                <span class="block truncate text-xs text-muted"
                  >{t.conclusion}</span
                >
              {/if}
            </span>
          </li>
        {/each}
      </ul>
    </div>
  </div>
{/if}
