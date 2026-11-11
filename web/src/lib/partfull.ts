/**
 * 取一个 part 的全文。
 *
 * 首屏那条路（`/api/session`）把 reasoning / tool 的正文截到 400 字 —— 实测一个
 * 10 条消息的窗口 190 KB 里 96% 是它们在屏幕上看不见的部分（reasoning 只渲染一行
 * 摘要，tool 只有一个折叠行）。截完大约 20 KB，Lighthouse 移动档的 LCP 从 3.8 s
 * 掉到 1.5 s 上下。
 *
 * 用户点开思考 / 工具面板时才来这里补剩下的。走本地回环几毫秒，用户感觉不到；
 * 远程连的时候也只是一次几十 KB 的请求，而且**只有真正点开才付**。
 *
 * 缓存按 part id 存一份：同一个面板关了又开不该再请求一次。缓存不设上限 ——
 * 一个会话里的 part 数量本来就是有限的，而且字符串是服务端那份的引用，不复制。
 * 会话切走时不必清：id 是 `r{i}` / 工具调用 id，跨会话理论上可能撞（`r3` 在
 * 两个会话里都有），所以**key 里带上会话**。
 */
import { store } from "./store.svelte.ts";

const cache = new Map<string, string>();
const inflight = new Map<string, Promise<string>>();

function key(id: string): string {
  return `${store.sessionId}\u0000${id}`;
}

/**
 * 拿全文。已经有了就直接返回；没有就请求一次（同一个 part 并发点两次只发一条）。
 *
 * 失败时**不抛** —— 返回空串，调用方继续显示那份截断的。面板里内容少一截，
 * 比弹一个错误框打断用户强。
 */
export async function partFull(id: string, kind: "output" | "input" = "output"): Promise<string> {
  const k = kind === "input" ? `in\u0000${key(id)}` : key(id);
  const hit = cache.get(k);
  if (hit !== undefined) return hit;
  const running = inflight.get(k);
  if (running) return running;

  const p = (async () => {
    try {
      const qs = new URLSearchParams({ id });
      if (kind === "input") qs.set("input", "1");
      // **不带空 session**：服务端把「给了 session 但查不到」当成 404，而空串
      // 就是查不到 —— 首屏 `store.sessionId` 还没同步上时就会踩到，表现为
      // 面板一直「载入全文…」。宁可让它退回「用当前活动会话」那条路（那正是
      // 用户点开面板时所在的那个会话）。
      if (store.sessionId) qs.set("session", store.sessionId);
      const r = await fetch(`/api/part?${qs}`);
      if (!r.ok) return "";
      const d = (await r.json()) as { text?: string };
      const text = d.text ?? "";
      cache.set(k, text);
      return text;
    } catch {
      return "";
    } finally {
      inflight.delete(k);
    }
  })();
  inflight.set(k, p);
  return p;
}
