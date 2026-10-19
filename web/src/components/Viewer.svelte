<script lang="ts">
  /**
   * 沙箱文件查看器：全屏覆盖层，从右边滑进来。
   *
   * 分五种：音视频用播放器；图片居中显示；**网页**（.html / 目录列表）放 iframe；
   * markdown（.md / .markdown）拉出来渲染成文档；其余文本（.sh / .py / .json / …）
   * 自己拉出来渲染成等宽文本。
   *
   * 为什么文本不丢给 iframe：`.sh` 要是发成 `application/x-sh`，WebView 不认，
   * iframe 里一片空白。服务端把这些发成 `text/plain` 了，
   * 能显示；但白底无样式、整段复制也难，所以还是自己渲染（见 TextView.svelte）。
   * 关闭：左上角返回箭头、返回键、按住标题栏往右拖。
   *
   * 网页在**另一个源**（`sandbox.localhost`，见 `sandboxPageUrl`）的 iframe 里：
   * 和 App 不同源，所以可以带 `allow-same-origin` —— 页面有自己正常的源，
   * localStorage / IndexedDB 能用，又碰不到 App 的凭据和 /api。万一拿到的是
   * 同源地址（不是 /sandbox 开头的），就不给 allow-same-origin：同源再给它，
   * 页面就能拆掉自己的 sandbox。
   */
  import { quintOut } from 'svelte/easing';
  import Img from './Img.svelte';
  import Icon from './Icon.svelte';
  import { onBack } from '../lib/overlay';
  import { dismissable, RELEASE } from '../lib/drag';
  import { viewer, closeViewer } from '../lib/viewer.svelte.ts';
  import { favorites } from '../lib/favorites.svelte.ts';
  import { probeBackground } from '../lib/pagebg';
  import { mediaKind, sandboxGuestPath, sandboxPageUrl } from '../lib/markdown';
  import TextView from './TextView.svelte';
  import MarkdownFile from './MarkdownFile.svelte';

  const IMAGE = /\.(png|jpe?g|gif|webp|svg|bmp|avif|ico)$/i;
  /** 明确是网页的扩展名 */
  const HTMLISH = /\.(x?html?|xhtml|php)$/i;
  /** 有没有「像扩展名」的后缀。没有的（`报告`、`LICENSE`）当网页/目录处理 */
  const HAS_EXT = /\.[a-z0-9]+$/i;
  /** markdown：渲染成文档看，而不是把源码丢进 TextView */
  const MARKDOWN = /\.(md|markdown|mdown|mkd)$/i;

  const path = $derived((viewer.url ?? '').split(/[?#]/)[0]);
  const isImage = $derived(viewer.kind === 'image' || IMAGE.test(path));
  const isMarkdown = $derived(!isImage && MARKDOWN.test(path));
  // 目录列表是服务端生成的 HTML：带结尾斜杠的、或者**没有扩展名**的都可能是它
  // （服务端对不带斜杠的目录会 302 到带斜杠的，fetch 会跟着走，拿回来是 HTML）。
  // 反过来，有扩展名的（.sh/.py/.json/…）才走文本视图。
  // 明说了是图片（没有扩展名的附件地址）就不是网页 —— 不然下面会白拉一次去探背景色
  const isWebPage = $derived(
    !isImage && (HTMLISH.test(path) || path.endsWith('/') || !HAS_EXT.test(path)),
  );
  const media = $derived(mediaKind(viewer.url ?? ''));

  /**
   * 这个页面对应的沙箱路径（收藏用）。不是网页 / 不是沙箱文件时为 null ——
   * 图片、音视频、文本、附件的 URL 上也没有「收藏」这回事。
   */
  const favPath = $derived(isWebPage ? sandboxGuestPath(viewer.url ?? '') : null);
  const faved = $derived(!!favPath && favorites.has(favPath));

  // 星是「亮 / 不亮」的，进来先要有一份收藏列表（模块级，抽屉那边也用这一份）
  $effect(() => {
    void favorites.load();
  });

  /**
   * 网页标题。iframe 在**另一个源**（`sandbox.localhost`）上，读不到里面的
   * `document`，所以自己再取一份源码，只抠 `<title>` —— 不解析别的。
   * 取不到（不是 HTML、网络失败）就用查看器给的标题（文件名）。
   *
   * 有了它，抽屉里那一行才是「销售报表」而不是「index.html」——
   * AI 做的页面文件名经常是 `index.html` / `out.html`，光看名字分不清。
   */
  async function pageTitle(url: string, fallback: string): Promise<string> {
    try {
      const r = await fetch(url, { credentials: 'omit' });
      if (r.ok) {
        const html = await r.text();
        const raw = /<title[^>]*>([\s\S]*?)<\/title>/i.exec(html)?.[1];
        if (raw) {
          // 实体（`&amp;` 之类）交给浏览器解。textarea 是 RCDATA：
          // 塞进去的标签只会当文字，不会执行 / 不会建节点。
          const t = document.createElement('textarea');
          t.innerHTML = raw;
          const title = t.value.replace(/\s+/g, ' ').trim();
          if (title) return title.slice(0, 120);
        }
      }
    } catch {
      /* 取不到就用文件名 */
    }
    return fallback;
  }

  /** 右上角那颗星：已收藏 = 取消，没收藏 = 收（标题顺手从页面里取一次） */
  async function toggleFavorite() {
    const path = favPath;
    if (!path) return;
    if (favorites.has(path)) await favorites.remove(path);
    else await favorites.add(path, await pageTitle(viewer.url ?? '', viewer.title));
  }

  /** iframe 实际加载的地址（沙箱网页换到独立的源上） */
  const frameSrc = $derived(viewer.url ? sandboxPageUrl(viewer.url) : '');
  /** 和 App 不同源才能放开 allow-same-origin（见文件头） */
  const isolated = $derived.by(() => {
    try {
      return new URL(frameSrc, location.href).origin !== location.origin;
    } catch {
      return false;
    }
  });
  /**
   * `allow-popups-to-escape-sandbox`：沙箱 iframe 里 `window.open` / `target=_blank`
   * 拿到的是 about:blank，真实地址要等页面自己写进去 —— 壳的 `onCreateWindow`
   * 接到的就是 about:blank，AI 写的网页里点外链等于没反应。放开这个 token，
   * 弹窗不受沙箱约束，URL 原样交到壳手上（见 MainActivity 的 onCreateWindow）。
   *
   * `allow-top-navigation-by-user-activation`：用户主动点的那一下允许导航顶层，
   * 主 WebView 的 `shouldOverrideUrlLoading` 就能接住，交给系统浏览器 / App
   * （起点、微信这些注册了 App Links 的会直接进 App）。
   */
  const frameSandbox = $derived(
    `allow-scripts allow-forms allow-modals allow-popups allow-popups-to-escape-sandbox allow-top-navigation-by-user-activation allow-downloads${isolated ? ' allow-same-origin' : ''}`,
  );

  $effect(() => onBack(closeViewer, drag));

  /**
   * 网页的底色。iframe 里那份文档是透明的（见 lib/pagebg.ts），
   * 所以这里既是 iframe 自己的背景色，也是页面没写背景时的可见底色。
   *
   *   `undefined` = 还没探到 → **不写背景（透明）**，露出 App 自己的底色；
   *                        写死白的话，深色页会先白后黑闪一下
   *   null      = 确定页面没写背景 → 白底（浏览器对无样式文档的默认）
   */
  let frameBg = $state<string | null | undefined>(undefined);

  $effect(() => {
    const url = viewer.url;
    if (!url || !isWebPage) return;
    frameBg = undefined;
    let alive = true;
    // iframe 立刻挂上，不阻塞加载（探测和页面加载并行跑）。
    // 探到之前是透明，所以最多是「深色底 → 页面的底色」，不会白闪。
    void probeBackground(url).then((bg) => {
      if (alive) frameBg = bg ?? '#fff';
    });
    return () => {
      alive = false;
    };
  });

  let box: HTMLElement | null = $state(null);
  let off = $state(0);
  let dragging = $state(false);

  const drag = {
    axis: 'x' as const,
    dir: 1 as const,
    size: () => box?.offsetWidth ?? 1,
    onStart: () => (dragging = true),
    onMove: (v: number) => (off = v),
    onEnd: (close: boolean) => {
      dragging = false;
      if (close) closeViewer();
      else off = 0;
    },
  };

  function slideIn(_: Element) {
    return {
      duration: 300,
      easing: quintOut,
      css: (_t: number, u: number) => `transform: translateX(${u * 100}%)`,
    };
  }

  function slideOut(_: Element) {
    const from = off;
    return {
      duration: 240 * (1 - from) + 80,
      easing: quintOut,
      css: (_t: number, u: number) => `transform: translateX(${(from + (1 - from) * u) * 100}%)`,
    };
  }
</script>

<section
  bind:this={box}
  class="fixed inset-0 z-[70] flex flex-col bg-ink will-change-transform"
  style:transform={off ? `translateX(${off * 100}%)` : null}
  style:transition={dragging ? 'none' : RELEASE}
  in:slideIn|global
  out:slideOut|global
  aria-label={viewer.title}
>
  <!-- 顶栏高度用 `--hdr`（= 对话页顶栏，36px + 安全区）：
       同一个 App 里三处顶栏（对话页 / 抽屉 / 查看器）应该一样高。

       **安全区要当内边距扣掉**：`--hdr` 里包含 `env(safe-area-inset-top)`，
       只写 `h-[var(--hdr)]` + items-center 的话，内容会被居中在整个高度里 ——
       真机（安全区 66px）上返回键落到 top=37，钻进状态栏里了（踩过）。
       对话页 header 就是这么写的（pt-4 + pb-4 + 28px 按钮）。 -->
  <header
    use:dismissable={drag}
    class="flex h-[var(--hdr)] shrink-0 items-center gap-2 px-3 pt-[env(safe-area-inset-top)]"
  >
    <button class="p-1 text-fg" aria-label="返回" onclick={closeViewer}>
      <Icon name="back" size={20} />
    </button>
    <span class="min-w-0 truncate text-base font-medium">{viewer.title}</span>
    <!-- 收藏：只有网页才有。已收藏是陶土橙的实心星，没收藏是 muted 的空心星 ——
         形状是同一个路径，只换 fill（见 Icon 的 `star`）。 -->
    {#if favPath}
      <button
        class="ml-auto shrink-0 p-1 {faved ? 'text-accent' : 'text-muted'}"
        aria-label={faved ? '取消收藏' : '收藏页面'}
        aria-pressed={faved}
        title={faved ? '取消收藏' : '收藏页面'}
        onclick={() => void toggleFavorite()}
      >
        <Icon name="star" size={20} filled={faved} />
      </button>
    {/if}
  </header>

  <div class="relative min-h-0 flex-1 border-t border-hairline">
    {#if media === 'video'}
      <div class="grid h-full place-items-center bg-black pb-[env(safe-area-inset-bottom)]">
        <!-- svelte-ignore a11y_media_has_caption -->
        <video src={viewer.url} controls autoplay playsinline class="max-h-full max-w-full"></video>
      </div>
    {:else if media === 'audio'}
      <div class="grid h-full place-items-center px-6">
        <audio src={viewer.url} controls autoplay class="w-full"></audio>
      </div>
    {:else if isImage}
      <div class="grid h-full place-items-center overflow-auto p-3 pb-[max(0.75rem,env(safe-area-inset-bottom))]">
        <Img src={viewer.url ?? ''} alt={viewer.title ?? ''} class="max-h-full max-w-full object-contain" />
      </div>
    {:else if isWebPage}
      <!-- 底色跟页面走（探不到就白底 —— 浏览器对无样式文档的默认）：
           内层文档 canvas 透明，所以这个背景就是它在页面画出来之前
           先上屏的颜色。探测结果没回来之前不写背景（透明）→ 露 App 底色 -->
      <iframe
        src={frameSrc}
        title={viewer.title}
        sandbox={frameSandbox}
        style:background={frameBg}
        class="absolute inset-0 size-full"
      ></iframe>
    {:else if isMarkdown}
      <MarkdownFile url={viewer.url ?? ''} />
    {:else}
      <TextView url={viewer.url ?? ''} />
    {/if}
  </div>
</section>
