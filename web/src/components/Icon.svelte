<script lang="ts">
  /**
   * 图标。全部内联 SVG，不引图标库。
   *
   * 尺寸按 Claude 界面实测：汉堡 17×15、状态点 19×19、圆形按钮内的图标 ~18。
   * 描边宽度 1.8（实测 2px @ DPR 2.625 ≈ 0.76css… 视觉上取 1.8 更像）。
   */
  type Name =
    | 'menu'
    | 'plus'
    | 'send'
    | 'stop'
    | 'mic'
    | 'refresh'
    | 'back'
    | 'chevron'
    | 'sparkle'
    | 'clock'
    | 'chat'
    | 'screen'
    | 'list'
    | 'pencil'
    | 'trash';

  let { name, size = 20, class: cls = '' }: { name: Name; size?: number; class?: string } =
    $props();
</script>

{#if name === 'menu'}
  <!-- 三条横线。实测 17.1 × 14.9 css，线间距 ~5.5 -->
  <svg width={size} height={size * 0.88} viewBox="0 0 17 15" fill="none" class={cls}>
    <path
      d="M1 1.2h15M1 7.5h15M1 13.8h15"
      stroke="currentColor"
      stroke-width="1.9"
      stroke-linecap="round"
    />
  </svg>
{:else if name === 'plus'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path d="M10 4v12M4 10h12" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" />
  </svg>
{:else if name === 'send'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path
      d="M10 16V4.6M5 9.6 10 4.6l5 5"
      stroke="currentColor"
      stroke-width="2.1"
      stroke-linecap="round"
      stroke-linejoin="round"
    />
  </svg>
{:else if name === 'stop'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="currentColor" class={cls}>
    <rect x="6" y="6" width="8" height="8" rx="1.6" />
  </svg>
{:else if name === 'mic'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <rect x="7.4" y="2.4" width="5.2" height="9.4" rx="2.6" stroke="currentColor" stroke-width="1.8" />
    <path d="M4.4 9.6a5.6 5.6 0 0 0 11.2 0M10 15.2V18" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" />
  </svg>
{:else if name === 'refresh'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path
      d="M16.4 10a6.4 6.4 0 1 1-1.9-4.5M16.4 3.6v4h-4"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linecap="round"
      stroke-linejoin="round"
    />
  </svg>
{:else if name === 'back'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path d="M12.5 4.5 7 10l5.5 5.5" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" />
  </svg>
{:else if name === 'pencil'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path
      d="M13.4 3.6l3 3L7.2 15.8l-3.9.9.9-3.9 9.2-9.2Z"
      stroke="currentColor"
      stroke-width="1.6"
      stroke-linejoin="round"
    />
  </svg>
{:else if name === 'trash'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path
      d="M3.8 5.6h12.4M8 5.6V3.8h4v1.8M5.4 5.6l.8 10.6h7.6l.8-10.6M8.4 8.6v4.8M11.6 8.6v4.8"
      stroke="currentColor"
      stroke-width="1.6"
      stroke-linecap="round"
      stroke-linejoin="round"
    />
  </svg>
{:else if name === 'chat'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path
      d="M3.4 9.6c0-3.2 2.9-5.8 6.6-5.8s6.6 2.6 6.6 5.8-2.9 5.8-6.6 5.8c-.8 0-1.6-.1-2.3-.4l-3.4 1.6.9-2.9a5.6 5.6 0 0 1-1.8-4.1Z"
      stroke="currentColor"
      stroke-width="1.6"
      stroke-linejoin="round"
    />
  </svg>
{:else if name === 'screen'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <rect x="5.2" y="2.4" width="9.6" height="15.2" rx="2.2" stroke="currentColor" stroke-width="1.6" />
    <path d="M8.6 15.4h2.8" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" />
  </svg>
{:else if name === 'list'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path
      d="M6.6 5.6h9.2M6.6 10h9.2M6.6 14.4h9.2M4 5.6h.01M4 10h.01M4 14.4h.01"
      stroke="currentColor"
      stroke-width="1.7"
      stroke-linecap="round"
    />
  </svg>
{:else if name === 'clock'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <circle cx="10" cy="10" r="7.4" stroke="currentColor" stroke-width="1.7" />
    <path d="M10 5.8V10l2.9 1.9" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" />
  </svg>
{:else if name === 'sparkle'}
  <!-- 四角星。用于「思考」那一行，跟工具调用的时钟图标区分开。 -->
  <svg width={size} height={size} viewBox="0 0 20 20" fill="currentColor" class={cls}>
    <path
      d="M10 3.6C10.7 7.4 12.6 9.3 16.4 10 12.6 10.7 10.7 12.6 10 16.4 9.3 12.6 7.4 10.7 3.6 10 7.4 9.3 9.3 7.4 10 3.6Z"
    />
  </svg>
{:else if name === 'chevron'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path d="M7.5 5 12.5 10l-5 5" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" />
  </svg>
{/if}
