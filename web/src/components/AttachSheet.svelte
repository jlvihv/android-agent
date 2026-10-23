<script lang="ts">
  /**
   * 「添加到会话」底部面板 —— 复用项目里那个 `Sheet`（同一个抓手、标题、
   * 遮罩、下拉/返回键关闭），只换正文。
   *
   * 样式对齐 Claude 的同一面板：三张等宽卡片，每张上面一个圆形图标槽、
   * 下面一个标签。**没有左上角的叉号** —— `Sheet` 本来就没有关闭按钮
   * （下拉、点遮罩、返回键都能关）。
   *
   * 目前只有相机 / 照片 / 文件。以后要加 Skills 之类，就往下面再放一行，
   * 不改这个三段式。
   */
  import Icon from "./Icon.svelte";
  import Sheet from "./Sheet.svelte";
  import { store } from "../lib/store.svelte.ts";

  let {
    onclose,
    onpick,
  }: {
    onclose: () => void;
    /** 用户选了哪一个入口。选完的收尾（关面板 / 拉起选择器）交给 Composer */
    onpick: (kind: "camera" | "photo" | "file") => void;
  } = $props();
</script>

<Sheet title="添加到会话" {onclose} hug>
  <div class="flex gap-3">
    <button
      onclick={() => onpick("camera")}
      class="flex flex-1 flex-col items-center gap-2.5 rounded-2xl bg-surface py-5
             text-sm text-fg"
    >
      <span class="grid size-11 place-items-center rounded-full bg-control">
        <Icon name="camera" size={20} />
      </span>
      相机
    </button>

    <button
      onclick={() => onpick("photo")}
      class="flex flex-1 flex-col items-center gap-2.5 rounded-2xl bg-surface py-5
             text-sm text-fg"
    >
      <span class="grid size-11 place-items-center rounded-full bg-control">
        <Icon name="image" size={20} />
      </span>
      照片
    </button>

    <!-- 只在模型吃 PDF 时出现：能力自动判定（models.dev），发了也必然 400 -->
    {#if store.capabilities.pdf}
      <button
        onclick={() => onpick("file")}
        class="flex flex-1 flex-col items-center gap-2.5 rounded-2xl bg-surface py-5
               text-sm text-fg"
      >
        <span class="grid size-11 place-items-center rounded-full bg-control">
          <Icon name="file" size={20} />
        </span>
        文件
      </button>
    {/if}
  </div>
</Sheet>
