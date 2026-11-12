/**
 * token 数的显示格式：**三位有效数字** + `k` / `M`。
 *
 * 面板和圆环是给人瞟一眼的，精确到个位只是噪声（真实值在 `/api/session` 里，
 * 随时能看）。一千以下原样给。
 *
 * 一处定义：上下文圆环（`ContextMeter`）和设置页的用量（`UsageSection`）以前
 * 各有一份**逐字相同**的实现 —— 那种地方一旦有人只改一份，同一组数字在
 * 两个界面上就对不上。
 */
const KILO = 1_000;
const MEGA = 1_000_000;

export function fmtCount(n: number): string {
  if (n >= MEGA) return `${(n / MEGA).toFixed(n >= MEGA * 10 ? 0 : 1)}M`;
  if (n >= KILO) return `${(n / KILO).toFixed(n >= KILO * 10 ? 0 : 1)}k`;
  return String(n);
}
