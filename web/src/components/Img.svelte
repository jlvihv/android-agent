<script lang="ts">
  /**
   * 图片。加载不出来（文件被删了、清扫收走了、外链失效）时换成我们自己的占位 ——
   * 浏览器默认那个裂图小方块跟界面一点都不搭，也看不出是怎么回事。
   *
   * - 默认：一个小胶囊「图裂图标 + 图片不见了」
   * - `compact`：只有图标，铺满 `class` 给的那个框（输入框里的缩略图那种小方块）
   *
   * `class` 给的是图片本身的样式；compact 占位也用它（尺寸 / 圆角 / 边框跟着走），
   * 默认占位只认自己的样式（图片的 max-h 之类对胶囊没意义）。
   */
  import Icon from "./Icon.svelte";

  let {
    src,
    alt = "",
    class: cls = "",
    compact = false,
    loading,
  }: {
    src: string;
    alt?: string;
    class?: string;
    compact?: boolean;
    loading?: "lazy" | "eager";
  } = $props();

  /** 哪个地址加载失败过：换了地址（同一个组件复用）就重新试 */
  let failed = $state<string | null>(null);
  const broken = $derived(failed === src);
  const label = $derived(alt ? `图片不见了：${alt}` : "图片不见了");
</script>

{#if broken}
  {#if compact}
    <span
      role="img"
      aria-label={label}
      class="grid place-items-center bg-hairline-soft text-muted {cls}"
    >
      <Icon name="imagebroken" size={16} />
    </span>
  {:else}
    <!-- 和附件那个「文件名胶囊」同一套规格（MessageList / Composer）：16px 图标、
         gap-1.5、rounded-card、px-3 py-2、text-meta —— 同一类东西长一个样 -->
    <span
      role="img"
      aria-label={label}
      class="inline-flex items-center gap-1.5 self-start rounded-card bg-hairline-soft px-3 py-2 text-meta text-muted"
    >
      <Icon name="imagebroken" size={16} class="shrink-0" />
      图片不见了
    </span>
  {/if}
{:else}
  <img {src} {alt} {loading} class={cls} onerror={() => (failed = src)} />
{/if}
