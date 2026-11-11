/**
 * 覆盖层的焦点管理。
 *
 * 背景（2026-11 补）：`Sheet` 一直写着 `role="dialog" aria-modal="true"`，但那只是
 * 「告诉」辅助技术这是个模态框 —— 浏览器**不会**因此限制焦点。实际行为是：
 *
 *   - Tab 能一路走到面板背后那些按钮上（看不见，但焦点真的在那儿）；
 *   - 面板打开时焦点留在原来的元素上，读屏不会念面板的标题；
 *   - 关掉之后焦点留在 `body`，用户得从头 Tab 一遍才回得到触发它的那个按钮。
 *
 * 这个模块补的就是这三件事。`aria-modal` 那条语义仍然有用（读屏据此忽略背景），
 * 但不够 —— 键盘焦点得自己管。
 *
 * **为什么不用 `<dialog showModal()>`**：全站覆盖层都靠 `lib/overlay.ts` 的
 * history 栈接安卓返回键、靠 `lib/drag.ts` 跟手拖动，而 `<dialog>` 的模态特性
 * 会把两者都盖掉（返回键变成浏览器自己的「关对话框」，拖动手势也被 top layer
 * 吃掉）。这不是「没用现代 API」，是那套 API 的前提（一个由浏览器画出来的模态层）
 * 和这里的架构冲突。原生能用的部分照样用：`inert` 下面就用上了。
 */

/** 能拿键盘焦点的元素。`visibility` / `display` 挡住的那些靠 `getClientRects` 排掉 */
const FOCUSABLE = [
  "a[href]",
  "button:not([disabled])",
  "input:not([disabled])",
  "select:not([disabled])",
  "textarea:not([disabled])",
  "summary",
  '[tabindex]:not([tabindex="-1"])',
].join(",");

function focusables(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    (el) =>
      el.getClientRects().length > 0 &&
      el.getAttribute("aria-hidden") !== "true" &&
      !el.closest("[inert]"),
  );
}

/**
 * 记下「打开前焦点在哪」，关掉时还回去。
 *
 * 给那些**不需要圈焦点**的层用：抽屉、查看器、设置页 —— 它们盖满屏幕，但背后
 * 没有用户会误以为还活着的可点区域，只需要一个「从哪儿来回哪儿去」。
 * 真正的模态圈焦点见下面的 `focusTrap`。
 *
 * 还焦点这一步如果目标已经不在 DOM 里了（比如点「新对话」导致头部按钮重渲染），
 * `isConnected` 为假 —— 那就放着不动，总比把焦点扔到 `body` 上强。
 */
export function focusRestore(node: HTMLElement) {
  void node; // 只是借它的生命周期
  const prev =
    document.activeElement instanceof HTMLElement
      ? document.activeElement
      : null;
  return {
    destroy() {
      if (prev?.isConnected) prev.focus({ preventScroll: true });
    },
  };
}

/**
 * 模态面板的焦点圈：进来先落到节点本身（`tabindex="-1"`，读屏念 `aria-label`），
 * Tab / Shift+Tab 在面板里循环，关掉时按 `focusRestore` 那样还回去。
 *
 * 不把焦点直接放到第一个按钮上：那样读屏第一句念的是「关闭」或者某个选项，
 * 用户不知道自己在哪个面板里。落在带 `aria-label={title}` 的容器上，`role=dialog`
 * 会让读屏先报标题（HTML 指南「把焦点移到新内容的开头」那条）。
 */
export function focusTrap(node: HTMLElement) {
  const restore = focusRestore(node);
  // 面板有自己的滑入动画，`preventScroll` 是为了别让浏览器顺手把底下那张
  // 已经滚了很久的消息列表拉走。
  node.focus({ preventScroll: true });

  function onKeydown(e: KeyboardEvent) {
    if (e.key !== "Tab") return;
    const list = focusables(node);
    if (list.length === 0) {
      // 面板里没有可聚焦的东西：焦点就该留在容器上，别让它跑到背后去
      e.preventDefault();
      node.focus({ preventScroll: true });
      return;
    }
    const first = list[0];
    const last = list[list.length - 1];
    const active = document.activeElement as HTMLElement | null;
    // 焦点还在容器上（刚打开）：Tab 从第一个开始，Shift+Tab 从最后一个开始
    if (active === node || !node.contains(active)) {
      e.preventDefault();
      (e.shiftKey ? last : first).focus({ preventScroll: true });
      return;
    }
    if (!e.shiftKey && active === last) {
      e.preventDefault();
      first.focus({ preventScroll: true });
    } else if (e.shiftKey && active === first) {
      e.preventDefault();
      last.focus({ preventScroll: true });
    }
  }

  node.addEventListener("keydown", onKeydown);
  return {
    destroy() {
      node.removeEventListener("keydown", onKeydown);
      restore.destroy();
    },
  };
}
