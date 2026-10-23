/**
 * 宽屏（桌面浏览器）= 侧边栏和对话**并排**；窄屏（手机）= 侧边栏还是抽屉。
 *
 * 为什么用 `matchMedia` 而不是纯 CSS 断点：这两种形态的差别不只是样式 ——
 * 并排的那一栏**不能**再注册返回键、**不能**再拦截拖拽、也**不能**有遮罩，
 * 而且头部的菜单按钮得消失，这些是行为，CSS 管不了。
 *
 * 断点 56rem（896px）：手机竖屏 ~411、横屏 ~890 都还在窄屏这一侧（抽屉），
 * 平板/电脑才分栏。
 *
 * 读 `wide.on` 就够了，它跟着窗口大小变。
 */
const mq = window.matchMedia("(min-width: 56rem)");

let on = $state(mq.matches);
mq.addEventListener("change", (e) => {
  on = e.matches;
});

export const wide = {
  get on(): boolean {
    return on;
  },
};
