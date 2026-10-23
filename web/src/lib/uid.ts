/**
 * `crypto.randomUUID()` 只在 secure context 里存在。
 * 我们的部署形态（http://127.0.0.1 或 https）都是 secure context，
 * 但用局域网 IP 调试时（http://192.168.x.x）就不是 —— 所以留个兜底。
 */
export function uid(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}
