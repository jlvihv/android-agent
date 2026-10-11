<script lang="ts">
  /**
   * 沙箱文件查看器：全屏覆盖层，从右边滑进来。
   *
   * HTML（和其他文本类文件）放进 iframe；图片居中显示；音视频用播放器。
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

  const IMAGE = /\.(png|jpe?g|gif|webp|svg|bmp|avif|ico)$/i;
  const isImage = $derived(IMAGE.test((viewer.url ?? '').split(/[?#]/)[0]));
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
    {:else}
      <!-- 白底：大多数网页默认是白底黑字，透出 App 的深色底反而看不清 -->
      <iframe
        src={viewer.url}
        title={viewer.title}
        sandbox="allow-scripts allow-forms allow-modals allow-popups allow-downloads"
        class="absolute inset-0 size-full bg-white"
      ></iframe>
    {/if}
  </div>
</section>
