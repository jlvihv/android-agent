<script lang="ts">
  /**
   * 开关（启用 / 停用）。`onchange` 返回 Promise 时，等它完成前不让再点 ——
   * 开关的状态要跟服务端一致，连点两下发出去两个相反的请求就乱了。
   */
  let {
    checked,
    label,
    onchange,
  }: {
    checked: boolean;
    /** 给读屏用（界面上开关旁边已经有名字了） */
    label: string;
    onchange: (next: boolean) => Promise<void> | void;
  } = $props();

  let busy = $state(false);

  async function toggle(e: MouseEvent) {
    // 开关常放在可点的行里：别把点击冒泡成「打开这一行」
    e.stopPropagation();
    if (busy) return;
    busy = true;
    try {
      await onchange(!checked);
    } finally {
      busy = false;
    }
  }
</script>

<button
  type="button"
  role="switch"
  aria-checked={checked}
  aria-label={label}
  disabled={busy}
  onclick={toggle}
  class="relative h-[26px] w-[44px] shrink-0 rounded-full transition-colors duration-150
         {checked ? 'bg-accent' : 'bg-active'} disabled:opacity-60"
>
  <span
    class="absolute top-[3px] left-[3px] size-5 rounded-full bg-fg transition-transform duration-150
           {checked ? 'translate-x-[18px]' : ''}"
  ></span>
</button>
