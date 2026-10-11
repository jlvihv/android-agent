<script lang="ts">
  /**
   * 沙箱文件查看器：全屏覆盖层，从右边滑进来。
   *
   * 分四种：音视频用播放器；图片居中显示；**网页**（.html / 目录列表）放 iframe；
   * 其余文本（.sh / .py / .json / …）自己拉出来渲染成等宽文本。
   *
   * 为什么文本不丢给 iframe：`.sh` 以前是 `application/x-sh`，WebView 不认，
   * iframe 里一片空白（用户实测）。服务端现在把这些发成 `text/plain` 了，
   * 能显示；但白底无样式、整段复制也难，所以还是自己渲染（见 TextView.svelte）。
   * 关闭：左上角返回箭头、返回键、按住标题栏往右拖。
   *
   * iframe 加了 sandbox 属性（不含 allow-same-origin），服务端也给
   * /sandbox/* 加了 CSP sandbox —— 页面是 opaque origin，碰不到 App 的
   * /api 和 WebSocket。
   */
  import { quintOut } from 'svelte/easing';
  import Icon from './Icon.svelte';
  import { onBack } from '../lib/overlay';
  import { dismissable, RELEASE } from '../lib/drag';
  import { viewer, closeViewer } from '../lib/viewer.svelte.ts';
  import { mediaKind } from '../lib/markdown';
  import TextView from './TextView.svelte';

  const IMAGE = /\.(png|jpe?g|gif|webp|svg|bmp|avif|ico)$/i;
  /** 明确是网页的扩展名 */
  const HTMLISH = /\.(x?html?|xhtml|php)$/i;
  /** 有没有「像扩展名」的后缀。没有的（`报告`、`LICENSE`）当网页/目录处理 */
  const HAS_EXT = /\.[a-z0-9]+$/i;

  const path = $derived((viewer.url ?? '').split(/[?#]/)[0]);
  const isImage = $derived(IMAGE.test(path));
  // 目录列表是服务端生成的 HTML：带结尾斜杠的、或者**没有扩展名**的都可能是它
  // （服务端对不带斜杠的目录会 302 到带斜杠的，fetch 会跟着走，拿回来是 HTML）。
  // 反过来，有扩展名的（.sh/.py/.json/…）才走文本视图。
  const isWebPage = $derived(HTMLISH.test(path) || path.endsWith('/') || !HAS_EXT.test(path));
  const media = $derived(mediaKind(viewer.url ?? ''));

  $effect(() => onBack(closeViewer));

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
  <header
    use:dismissable={drag}
    class="flex shrink-0 items-center gap-2 px-3 pt-[calc(0.5rem+env(safe-area-inset-top))] pb-2"
  >
    <button class="p-1.5 text-fg" aria-label="返回" onclick={closeViewer}>
      <Icon name="back" size={20} />
    </button>
    <span class="min-w-0 truncate text-base font-medium">{viewer.title}</span>
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
        <img src={viewer.url} alt={viewer.title} class="max-h-full max-w-full object-contain" />
      </div>
    {:else if isWebPage}
      <!-- 白底：大多数网页默认是白底黑字，透出 App 的深色底反而看不清 -->
      <iframe
        src={viewer.url}
        title={viewer.title}
        sandbox="allow-scripts allow-forms allow-modals allow-popups allow-downloads"
        class="absolute inset-0 size-full bg-white"
      ></iframe>
    {:else}
      <TextView url={viewer.url ?? ''} />
    {/if}
  </div>
</section>
