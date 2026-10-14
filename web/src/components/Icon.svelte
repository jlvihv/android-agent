<script lang="ts">
  /**
   * 图标。全部内联 SVG，不引图标库。
   *
   * **统一规格**（同一行放不同图标，光学尺寸才对得上）：
   *   - `viewBox="0 0 20 20"`，正方形画布 —— 一律 20，没有例外
   *     （`gear` 原来是 24，描边实际只有 1.58 px，跟谁都不齐）
   *   - `stroke-width="1.8"` —— 一律 1.8，没有例外
   *     （原来 1.6/1.7/1.8/1.9/2.1 混着用）
   *   - 线端/拐角 `round`
   *   - **描边型**图标的墨迹（几何 + 半个描边）都在 13.6–16.8 之间；
   *     填充型（`stop` `sparkle`）没有描边，靠外框对齐
   *   - 故意画小的只有两个“小尖角”`back` / `chevron` —— 它们贴着文字用，
   *     撑满画布反而把行高顶开
   *
   * 各图标墨迹（上面这条规矩的实测值，20px 渲染时就是像素）：
   *   gear 16.8 · newchat 16.8 · clock 16.6 · menu 16.2×10.2 · phone 10×16.6
   *   computer 16.2×15.5 · sparkle 15.8
   *   chat 15.0 · pencil 14.9 · trash 14.6 · plus 14.2
   *   send 11.8×13.2 · stop 8 · back 7.3×12.8 · chevron 6.8×11.8
   */
  type Name =
    | 'menu'
    | 'newchat'
    | 'plus'
    | 'send'
    | 'stop'
    | 'back'
    | 'chevron'
    | 'sparkle'
    | 'clock'
    | 'chat'
    | 'pencil'
    | 'trash'
    | 'gear'
    | 'camera'
    | 'image'
    | 'file'
    | 'check'
    | 'globe'
    | 'phone'
    | 'computer'
    | 'close';

  let { name, size = 20, class: cls = '' }: { name: Name; size?: number; class?: string } =
    $props();
</script>

{#if name === 'menu'}
  <!-- 两条横线：上一条满宽，下一条 3/5（14.4 × 0.6 = 8.64）。
       比三条视觉噪点少，也比三条更明确地“只是个菜单”。
       行距 8.4（5.8 / 14.2）：最初写的 6.4 贴得太近，两条像一条粗线。 -->
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path
      d="M2.8 5.8h14.4M2.8 14.2h8.64"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linecap="round"
    />
  </svg>
{:else if name === 'newchat'}
  <!-- 新对话：圆气泡 + 左下小尾巴 + 里面一个加号。
       经典 message-circle 造型（半径 7.5 的圆），等比缩到 20 画布：
       一条路径画完（长弧 + 尾巴三角形闭合），不用两个图形叠，
       交接处不会出现双重描边。
       尾巴尖端停在 y=17.2 —— **不越过圆的底部**（圆心 y=10 + 半径 7.5 = 17.5），
       否则加上描边会有一小截垂到圆下面去（最初的 18.33 就是这样）。 -->
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path
      d="M6.58 16.67A7.5 7.5 0 1 0 3.33 13.42L2.75 17.2Z"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linejoin="round"
    />
    <path d="M10 7.2v5.6M7.2 10h5.6" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" />
  </svg>
{:else if name === 'plus'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path d="M10 3.8v12.4M3.8 10h12.4" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" />
  </svg>
{:else if name === 'send'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path
      d="M10 16V4.6M5 9.6 10 4.6l5 5"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linecap="round"
      stroke-linejoin="round"
    />
  </svg>
{:else if name === 'stop'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="currentColor" class={cls}>
    <rect x="6" y="6" width="8" height="8" rx="1.6" />
  </svg>
{:else if name === 'back'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path d="M12.5 4.5 7 10l5.5 5.5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" />
  </svg>
{:else if name === 'chevron'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path d="M7.5 5 12.5 10l-5 5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" />
  </svg>
{:else if name === 'check'}
  <!-- 对勾。描边型，墨迹 11.6 × 8.2，跟同行的文字居中。
       竖笔比横笔稍短一点点（4.7 vs 4.9），视觉上两段才等长。 -->
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path
      d="M4.2 10.6 8 14.3l7.8-8.6"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linecap="round"
      stroke-linejoin="round"
    />
  </svg>
{:else if name === 'pencil'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path
      d="M13.4 3.6l3 3L7.2 15.8l-3.9.9.9-3.9 9.2-9.2Z"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linejoin="round"
    />
  </svg>
{:else if name === 'trash'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path
      d="M3.6 5.8h12.8M8 5.8V4h4v1.8M5.3 5.8l.8 10.4h7.8l.8-10.4M8.4 8.8v4.6M11.6 8.8v4.6"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linecap="round"
      stroke-linejoin="round"
    />
  </svg>
{:else if name === 'chat'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path
      d="M3.4 9.6c0-3.2 2.9-5.8 6.6-5.8s6.6 2.6 6.6 5.8-2.9 5.8-6.6 5.8c-.8 0-1.6-.1-2.3-.4l-3.4 1.6.9-2.9a5.6 5.6 0 0 1-1.8-4.1Z"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linejoin="round"
    />
  </svg>
{:else if name === 'clock'}
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <circle cx="10" cy="10" r="7.4" stroke="currentColor" stroke-width="1.8" />
    <path d="M10 5.8V10l2.9 1.9" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" />
  </svg>
{:else if name === 'sparkle'}
  <!-- 四角星。用于「思考」那一行，跟工具调用那行的时钟图标并排，
       所以外框要对齐：老版本只有 12.8，比时钟（16.6）小一圈，
       同一列里看着一个瘦一个胖。现在按 1.234 倍从中心放大到 15.8。 -->
  <svg width={size} height={size} viewBox="0 0 20 20" fill="currentColor" class={cls}>
    <path d="M10 2.1C10.86 6.79 13.21 9.14 17.9 10 13.21 10.86 10.86 13.21 10 17.9 9.14 13.21 6.79 10.86 2.1 10 6.79 9.14 9.14 6.79 10 2.1Z" />
  </svg>
{:else if name === 'gear'}
  <!-- 齿轮。抽屉左下角那个「设置」入口用。
       经典 cog 造型（8 齿圆角）缩到 20 画布：以 24 画布上的原图中心 (12,12) 为基准
       整体 ×0.68 并平移到 (10,10)，孔半径 2.108，墨迹 16.8 —— 和 newchat 完全同尺寸。
       为什么不自己画 8 齿梯形：20 画布里齿深只剩 2 上下，配上 1.8 的描边，
       齿和齿根会糊成一个圆疙瘩（试过三版都一样），圆角齿廓才立得住。 -->
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <circle cx="10" cy="10" r="2.108" stroke="currentColor" stroke-width="1.8" />
    <path
      d="M15.032 12.04a1.122 1.122 0 0 0 0.224 1.238l0.041 0.041a1.36 1.36 0 0 1 0 1.924a1.36 1.36 0 0 1-1.924 0l-0.041-0.041a1.122 1.122 0 0 0-1.238-0.224a1.122 1.122 0 0 0-0.68 1.027V16.12a1.36 1.36 0 0 1-1.36 1.36a1.36 1.36 0 0 1-1.36-1.36v-0.061A1.122 1.122 0 0 0 7.96 15.032a1.122 1.122 0 0 0-1.238 0.224l-0.041 0.041a1.36 1.36 0 0 1-1.924 0a1.36 1.36 0 0 1 0-1.924l0.041-0.041a1.122 1.122 0 0 0 0.224-1.238a1.122 1.122 0 0 0-1.027-0.68H3.88a1.36 1.36 0 0 1-1.36-1.36a1.36 1.36 0 0 1 1.36-1.36h0.061A1.122 1.122 0 0 0 4.968 7.96a1.122 1.122 0 0 0-0.224-1.238l-0.041-0.041a1.36 1.36 0 0 1 0-1.924a1.36 1.36 0 0 1 1.924 0l0.041 0.041a1.122 1.122 0 0 0 1.238 0.224H7.96a1.122 1.122 0 0 0 0.68-1.027V3.88a1.36 1.36 0 0 1 1.36-1.36a1.36 1.36 0 0 1 1.36 1.36v0.061a1.122 1.122 0 0 0 0.68 1.027a1.122 1.122 0 0 0 1.238-0.224l0.041-0.041a1.36 1.36 0 0 1 1.924 0a1.36 1.36 0 0 1 0 1.924l-0.041 0.041a1.122 1.122 0 0 0-0.224 1.238V7.96a1.122 1.122 0 0 0 1.027 0.68H16.12a1.36 1.36 0 0 1 1.36 1.36a1.36 1.36 0 0 1-1.36 1.36h-0.061a1.122 1.122 0 0 0-1.027 0.68Z"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linecap="round"
      stroke-linejoin="round"
    />
  </svg>
{:else if name === 'camera'}
  <!-- 相机：机身 + 顶部取景器凸起 + 镜头。
       机身 14.4×10.6（x 2.8–17.2，y 5.4–16），墨迹 16.4×13。 -->
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path
      d="M7.2 5.4 8.1 3.9h3.8l.9 1.5h3.6a1 1 0 0 1 1 1v9.1a1 1 0 0 1-1 1H3.6a1 1 0 0 1-1-1V6.4a1 1 0 0 1 1-1h3.6Z"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linejoin="round"
    />
    <circle cx="10" cy="10.9" r="2.7" stroke="currentColor" stroke-width="1.8" />
  </svg>
{:else if name === 'image'}
  <!-- 图片：外框 + 左下小太阳 + 右下斜山。和 camera 同为 16.4 墨迹。 -->
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <rect x="2.8" y="3.8" width="14.4" height="12.4" rx="2" stroke="currentColor" stroke-width="1.8" />
    <circle cx="7.3" cy="8.2" r="1.4" stroke="currentColor" stroke-width="1.8" />
    <path d="m4.6 15.4 4-4 3.2 3.2 2.2-2.2 2 2" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" />
  </svg>
{:else if name === 'file'}
  <!-- 文件：右上折角 + 左下折角线。墨迹 12.8×14.6。 -->
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path
      d="M11.6 2.7H5.9a1.6 1.6 0 0 0-1.6 1.6v11.4a1.6 1.6 0 0 0 1.6 1.6h8.2a1.6 1.6 0 0 0 1.6-1.6V6.8l-4.1-4.1Z"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linejoin="round"
    />
    <path d="M11.6 2.7v4.1h4.1" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round" />
  </svg>
{:else if name === 'globe'}
  <!-- 地球：外圆 ∅15（墨迹 15 + 1.8 = 16.8，跟 gear/newchat 同档），
       里头一条赤道 + 一个经线圈（rx 3.6 ≈ 外圆半径的一半，看得到“球”感）。 -->
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <circle cx="10" cy="10" r="7.5" stroke="currentColor" stroke-width="1.8" />
    <ellipse cx="10" cy="10" rx="3.6" ry="7.5" stroke="currentColor" stroke-width="1.8" />
    <path d="M2.5 10h15" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" />
  </svg>
{:else if name === 'phone'}
  <!-- 手机：竖圆角矩形 + 底部一条横线。宽 8.2 是手机的天然比例（进不了
       13.6–16.8 那一档，跟 menu 的 10.2 是一个道理），高度 14.8 对齐 clock
       的 16.6 墨迹 —— 侧栏底栏那个小标签里，它和 computer 是同一行的邻居，
       高度对齐才不会一个高一个矮。 -->
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <rect x="5.9" y="2.6" width="8.2" height="14.8" rx="2.2" stroke="currentColor" stroke-width="1.8" />
    <path d="M8.9 15.2h2.2" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" />
  </svg>
{:else if name === 'computer'}
  <!-- 显示器：屏 + 立柱 + 底座。墨迹 16.2×15.5（和 menu 同为 16.2 宽）。
       底边比顶边略多让一点（2.3 / 2.2）—— 立柱和底座把重心往下带，
       几何居中反而看着偏上。 -->
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <rect x="2.8" y="3.2" width="14.4" height="10.4" rx="1.8" stroke="currentColor" stroke-width="1.8" />
    <path d="M10 13.6v3.3M6.7 16.9h6.6" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" />
  </svg>
{:else if name === 'close'}
  <!-- 叉：两条对角线，墨迹 11.6×11.6（跟 send 同档）。 -->
  <svg width={size} height={size} viewBox="0 0 20 20" fill="none" class={cls}>
    <path d="M4.6 4.6 15.4 15.4M15.4 4.6 4.6 15.4" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" />
  </svg>
{/if}
