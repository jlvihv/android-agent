/**
 * 取一个 part 点开面板时需要的东西（一次请求全拿）。
 *
 * 首屏那条路（`/api/session`）把 reasoning / tool 的正文截到 400 字、工具参数
 * 截到 200 字/字段 —— 实测一个 10 条消息的窗口 190 KB 里 96% 是它们在屏幕上
 * 看不见的部分（reasoning 只渲染一行摘要，tool 只有一个折叠行）。截完不到一半。
 *
 * 用户点开思考 / 工具面板时才来这里补剩下的。走本地回环几毫秒，用户感觉不到；
 * 远程连的时候也只是一次请求，而且**只有真正点开才付**。
 *
 * **一次请求就够**。早先拆成「取 output」和「取 input」两条，输入输出都长的那种
 * 工具点一下会并发两条往返 —— 同一份数据的两个字段，没理由拆两次。现在服务端
 * 按 part 一次把**所有被截过的字段**都给回来（`GET /api/part` 不带参数就是要
 * 全部），前端只发一条。
 *
 * 缓存按 part id 存一份：同一个面板关了又开不该再请求一次。缓存不设上限 ——
 * 一个会话里的 part 数量本来就是有限的。
 *
 * **key 里必须带会话**：id 是 `r{i}` / `t{i}`（`ChatMessage` 的下标）和工具调用 id，
 * 前者跨会话一定会撞（两个会话都有 `r3`）。会话由调用方传进来 —— 这里**不读全局
 * 状态**：面板是点开那一刻才查缓存，而全局那份会话靠 `$effect` 同步，两者不同步时
 * 就会拿错会话的全文字。
 */

/** 一个 part 的全部完整内容：`text` 是推理/正文，`output` 是工具输出，`input` 是工具参数 */
export type PartFull = {
  text?: string;
  output?: string;
  input?: Record<string, unknown>;
};

const cache = new Map<string, PartFull>();
const inflight = new Map<string, Promise<PartFull>>();

function key(session: string, id: string): string {
  return `${session}\u0000${id}`;
}

/**
 * 拿全文。已经有了就直接返回；没有就请求一次（同一个 part 并发点两次只发一条）。
 *
 * 失败时**不抛** —— 返回空对象，调用方继续显示那份截断的。面板里内容少一截，
 * 比弹一个错误框打断用户强。
 */
export async function partFull(id: string, session: string): Promise<PartFull> {
  const k = key(session, id);
  const hit = cache.get(k);
  if (hit !== undefined) return hit;
  const running = inflight.get(k);
  if (running) return running;

  // `session` 现在是**必填**：服务端没有「当前会话」可以兜底了（见 core 的
  // `SessionStore` 注释），而 part id（`r3`）离开会话就没有意义。
  // 真为空（理论上到不了：面板是从消息里点开的，那时一定有会话）就返回空对象，
  // 让面板显示截断的那份，别发一个必然 400 的请求。
  const p = (async (): Promise<PartFull> => {
    try {
      if (!session) return {};
      const qs = new URLSearchParams({ id, session });
      const r = await fetch(`/api/part?${qs}`);
      if (!r.ok) return {};
      const d = (await r.json()) as PartFull;
      cache.set(k, d);
      return d;
    } catch {
      return {};
    } finally {
      inflight.delete(k);
    }
  })();
  inflight.set(k, p);
  return p;
}
