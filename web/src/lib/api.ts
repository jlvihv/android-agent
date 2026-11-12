/**
 * 核心的 JSON 接口。**一处**处理「不是 2xx」和「读错误正文」。
 *
 * 以前这 20 多个调用点各抄一遍 `if (!r.ok) throw new Error((await r.text()) || \`HTTP ${r.status}\`)`
 * —— 抄漏一个就是「失败了但界面没反应」，而错误正文往往正是核心给的那句人话
 * （「Key 无效」之类），不能被 `HTTP 500` 盖掉。
 *
 * 只给**一次请求一个 JSON** 的地方用。流（`/api/chat`）、要读源码拼 HTML、
 * 要自己兜错误状态的那几处（`models` / `machines` / `favorites` 的乐观更新）
 * 仍然自己 fetch —— 它们的错误处理本来就不是「抛出去」。
 */

/** 失败：`message` 就是能直接给人看的那句话（服务端正文优先，其次是状态码） */
export class ApiError extends Error {
  status: number;
  constructor(status: number, body: string) {
    super(body.trim() || `HTTP ${status}`);
    this.status = status;
  }
}

export async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const r = await fetch(path, init);
  if (!r.ok) throw new ApiError(r.status, await r.text());
  return (await r.json()) as T;
}

/** POST 一个 JSON body。`undefined` 就不带 body（有些端点只要方法） */
export function post<T>(path: string, body?: unknown): Promise<T> {
  return api<T>(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

/** DELETE，没有 body。 */
export function del<T>(path: string): Promise<T> {
  return api<T>(path, { method: "DELETE" });
}
