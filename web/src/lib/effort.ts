/**
 * 思考强度档位。**一个地方定义，模型面板和强度面板共用**。
 *
 * `value` 直接透传给端点：
 *   - Chat Completions → 顶层 `reasoning_effort`
 *   - Responses        → `reasoning.effort`
 * `null` = 这个参数一个字都不发（各家的默认档）。
 *
 * 不做白名单校验：各家档位集合不一样（OpenAI 有 `minimal`，某些网关只有 `low/high`），
 * 端点不认识就忽略 —— 我们只负责把用户选的递过去。
 */
export const EFFORTS = [
  { value: null, label: "默认" },
  { value: "minimal", label: "极低" },
  { value: "low", label: "低" },
  { value: "medium", label: "中等" },
  { value: "high", label: "高" },
] as const;

export type EffortValue = (typeof EFFORTS)[number]["value"];

/** 值 → 显示名。认不出来的（比如 CLI 手填的）原样显示，别显示成「默认」骗人。 */
export function effortLabel(v: string | null | undefined): string {
  return EFFORTS.find((e) => e.value === (v ?? null))?.label ?? String(v);
}
