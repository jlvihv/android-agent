/**
 * 跟手拖拽关闭：抽屉往左收、底部面板往下收。
 *
 * 为什么不用 pointer 事件：手指一旦被判定为"滚动"，浏览器就发
 * pointercancel 把手势抢走 —— 在抽屉的会话列表上横着拖、在面板正文上
 * 往下拉，都会拖到一半断掉。touch 事件可以在第一次 touchmove 时
 * preventDefault，认定方向之后整段手势都归我们。鼠标（桌面调试）走
 * pointer 事件，没有这个问题。
 *
 * 松手判定**看最后一小段的速度方向**，不看整段平均：
 *   - 最后还在往关的方向走 → 关（哪怕拉得很慢、很短）
 *   - 最后在往回走       → 留
 *   - 停住了再松手       → 看位置，过 40% 就关
 * 原来是"拖过 1/3 或者 250ms 内甩出去"，慢慢拉过一半松手也会弹回来，
 * 手感就是"不受控制"。
 */

export type DismissOpts = {
  axis: 'x' | 'y';
  /** 关闭方向：-1 = 往左/上，1 = 往右/下 */
  dir: -1 | 1;
  /** 被拖动的尺寸（宽或高），把位移换算成 0..1 */
  size: () => number;
  /** 这次手势能不能接管。比如面板正文没滚到顶，往下拉应该是滚正文 */
  canStart?: (target: EventTarget | null) => boolean;
  /** 认定开始拖了（比如取消长按计时） */
  onStart?: () => void;
  /** 0 = 原位，1 = 完全收起 */
  onMove: (p: number) => void;
  onEnd: (close: boolean) => void;
};

/** 手指移动多少像素之后才认方向 */
const SLOP = 6;
/** 松手速度阈值（像素/毫秒，≈50px/s）。低于它算"停住了"。
 *  故意设得很低：慢慢往下拉也是"想关"，只有真停住了才看位置 */
const FLING = 0.05;
/** 停住时过这个比例就关 */
const HALF = 0.4;

export function dismissable(node: HTMLElement, opts: DismissOpts) {
  let o = opts;
  let state: 'idle' | 'pending' | 'drag' | 'native' = 'idle';
  let x0 = 0;
  let y0 = 0;
  /** 认定方向那一刻的主轴位移 —— 从这里开始算，不然一开拖就跳 SLOP 像素 */
  let base = 0;
  let target: EventTarget | null = null;
  let p = 0;
  /** 最近的 (时间, 位移像素) 采样，算松手速度用 */
  let samples: [number, number][] = [];
  /** 拖完抬手会触发一次 click，吞掉它 */
  let swallowClick = false;

  function begin(x: number, y: number, t: EventTarget | null) {
    state = 'pending';
    x0 = x;
    y0 = y;
    target = t;
    samples = [];
  }

  /** 返回 true 表示这一下归我们，调用方要 preventDefault */
  function move(x: number, y: number, now: number): boolean {
    if (state === 'idle' || state === 'native') return false;
    const dx = x - x0;
    const dy = y - y0;
    const main = o.axis === 'x' ? dx : dy;
    const cross = o.axis === 'x' ? dy : dx;

    if (state === 'pending') {
      if (Math.hypot(dx, dy) < SLOP) return false;
      const ours =
        Math.abs(main) > Math.abs(cross) && main * o.dir > 0 && (o.canStart?.(target) ?? true);
      if (!ours) {
        state = 'native';
        return false;
      }
      state = 'drag';
      base = main;
      o.onStart?.();
    }

    const px = Math.max(0, (main - base) * o.dir);
    p = Math.min(1, px / Math.max(1, o.size()));
    samples.push([now, px]);
    while (samples.length > 2 && now - samples[0][0] > 100) samples.shift();
    o.onMove(p);
    return true;
  }

  function end() {
    if (state !== 'drag') {
      state = 'idle';
      return;
    }
    state = 'idle';
    swallowClick = true;
    setTimeout(() => (swallowClick = false), 0);

    let v = 0;
    if (samples.length >= 2) {
      const [t1, a] = samples[0];
      const [t2, b] = samples[samples.length - 1];
      v = (b - a) / Math.max(1, t2 - t1);
    }
    const close = v > FLING ? true : v < -FLING ? false : p > HALF;
    o.onEnd(close);
  }

  // —— 触摸 ——
  const ts = (e: TouchEvent) => {
    if (e.touches.length !== 1) return void (state = 'native');
    const t = e.touches[0];
    begin(t.clientX, t.clientY, e.target);
  };
  const tm = (e: TouchEvent) => {
    const t = e.touches[0];
    if (t && move(t.clientX, t.clientY, e.timeStamp) && e.cancelable) e.preventDefault();
  };
  const te = () => end();

  // —— 鼠标（桌面调试）——
  const pd = (e: PointerEvent) => {
    if (e.pointerType !== 'mouse' || e.button !== 0) return;
    begin(e.clientX, e.clientY, e.target);
    const pm = (ev: PointerEvent) => void move(ev.clientX, ev.clientY, ev.timeStamp);
    const pu = () => {
      removeEventListener('pointermove', pm);
      removeEventListener('pointerup', pu);
      end();
    };
    addEventListener('pointermove', pm);
    addEventListener('pointerup', pu);
  };

  const click = (e: MouseEvent) => {
    if (!swallowClick) return;
    swallowClick = false;
    e.stopPropagation();
    e.preventDefault();
  };

  node.addEventListener('touchstart', ts, { passive: true });
  node.addEventListener('touchmove', tm, { passive: false });
  node.addEventListener('touchend', te);
  node.addEventListener('touchcancel', te);
  node.addEventListener('pointerdown', pd);
  node.addEventListener('click', click, true);

  return {
    update(next: DismissOpts) {
      o = next;
    },
    destroy() {
      node.removeEventListener('touchstart', ts);
      node.removeEventListener('touchmove', tm);
      node.removeEventListener('touchend', te);
      node.removeEventListener('touchcancel', te);
      node.removeEventListener('pointerdown', pd);
      node.removeEventListener('click', click, true);
    },
  };
}

/** 松手后的回弹/收起曲线：先快后慢，接得住手指的速度 */
export const RELEASE = 'transform 280ms cubic-bezier(0.22, 1, 0.36, 1)';
/** 面板内部换页（横移 + 标题飞入）的时长。300 太急，看不清是从哪页到哪页 */
export const PAGE_MS = 450;
