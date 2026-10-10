/**
 * 覆盖层接入 Android 返回键。
 *
 * 抽屉/面板是 JS 覆盖层，系统返回键不知道它们存在 —— 不处理的话按返回
 * 会直接退出 App。做法：打开时 push 一条 history，返回键触发 popstate 时
 * 关掉**最上面**那一层。
 *
 * 之前是 App 里统一管，但只覆盖了抽屉和模型设置；消息列表里的思考/工具
 * 面板没接，按返回直接退出。而且"新建会话"这类不走 history 的关闭会留下
 * 脏记录，下次返回要按两下。现在每个覆盖层自己登记：
 *
 *   - 返回键 → 弹出栈顶，调它的 close
 *   - 任何别的方式关掉（点遮罩、拖走、代码里直接卸载）→ 卸载时 release()，
 *     自己把多出来的那条 history 退掉；这次 popstate 要跳过，别误关下一层
 */

const stack: (() => void)[] = [];
let skip = 0;

if (typeof window !== 'undefined') {
  addEventListener('popstate', () => {
    if (skip > 0) {
      skip--;
      return;
    }
    stack.pop()?.();
  });
}

/** 登记一层覆盖层。返回 release，在卸载时调（放进 $effect 的清理函数里正好）。 */
export function onBack(close: () => void): () => void {
  history.pushState({ overlay: stack.length + 1 }, '');
  stack.push(close);
  return () => {
    const i = stack.lastIndexOf(close);
    if (i === -1) return; // 已经被返回键弹掉了
    stack.splice(i, 1);
    skip++;
    history.back();
  };
}
