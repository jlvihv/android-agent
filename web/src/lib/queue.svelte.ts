/**
 * 「排队中」那条虚线气泡的**本地镜像**（服务端队列的画法）。
 *
 * 队列本身在核心（`AppState::queued`）：`POST /api/chat` 碰上这个会话正跑着
 * 就把它排进去，正在跑的那个回合**一空下来**（步边界）就把它交给模型。
 * 为什么不在前端排：前端那个队列活在 WebView 里，App 切到后台、WebView 被冻住
 * 或回收，这条消息就永远发不出去了（见 docs/05 的「排队」）。
 *
 * 这一份只负责**画**（虚线气泡 + 左边那个 ×），三个来源：
 *
 * 1. `send` 里刚发出去、还没听到回话的那条（`pending: true`）—— 服务端说
 *    「排队了」就坐实，说「开跑了」就翻进正文（见 `Live#deliver`）。
 * 2. 服务端把排队的消息交给模型时推的 `data-queued-delivered`：从这里挪进正文。
 * 3. `load` 每次按 `/api/session` 的 `queued` **整份重来** —— App 被回收过 /
 *    换了设备 / 界面刚刷新时，那几条就是从这儿回来的。
 *
 * 从 `chat.svelte.ts` 拆出来：它和**流**没关系（只是「服务端说了什么」的镜子），
 * 却占了 `Chat` 里一大截。单独一个文件，读的人一眼看完。
 */

import { post } from "./api";
import type { Message, Part } from "./sse";

/**
 * 一条排队等着交给模型的消息。
 *
 * 每条记着自己属于哪个会话（`sid`）：回合在后台跑着时用户可能已经切走了。
 */
export type Queued = {
  id: string;
  sid: string;
  parts: Part[];
  pending?: boolean;
  /**
   * 这条不是用户写的，是服务端别的来源投进来的（`ChatMessage::User.origin`）。
   *
   * 现在有两种：子会话的回执（`fromWorker`）和定时器到点（`fromSchedule`）。
   * 服务端 `queued_view` 就在给这两个键（和已落历史的那条用同一组），**必须
   * 一路带到画的地方** —— 不然同一个东西在「排队中」是一个用户气泡、交出去之后
   * 才变成「子会话报告」/「定时任务」，凭空换个样子。
   */
  fromWorker?: string;
  fromSchedule?: string;
};

export class QueueMirror {
  /** 所有会话的排队项（画的时候按 `sid` 过滤） */
  items = $state<Queued[]>([]);

  /** 某条会话的那几条（`App` 的 `queued` / `queuedMsgs` 读它） */
  of(sid: string): Queued[] {
    return this.items.filter((q) => q.sid === sid);
  }

  /** 刚发出去、还没听到回话的那条（服务端下一份就会报上它，或者它已经开跑了） */
  add(q: Queued): void {
    this.items = [...this.items, q];
  }

  /**
   * 服务端报了这个会话的队列：**只换这个会话的那几条**（别的会话的排队项留着 ——
   * 切回去之前不会再拉它们）。
   */
  replace(sid: string, next: Message[]): void {
    // `pending` 的那几条是刚发出去、还没听到回话的，留着 —— 服务端下一份就会
    // 报上它们（或者它们已经开跑了）。
    const hoping = this.items.filter((q) => q.pending && q.sid === sid);
    const merged = [
      ...this.items.filter((q) => q.sid !== sid),
      ...next.map((m): Queued => ({
        id: m.id,
        sid,
        parts: m.parts,
        fromWorker: m.fromWorker,
        fromSchedule: m.fromSchedule,
      })),
      ...hoping,
    ];
    // **没变就别换数组**：`load()` 每 1.5 秒轮询一次，服务端绝大多数时候回的是
    // 同一个空队列；无条件换新数组会让依赖它的 `$derived`（`App` 里的
    // `queued` / `queuedMsgs`）每次轮询都重算一遍（实测 3 次轮询 → 3 次重算）。
    // 判据只看 id + pending：内容真的变了，这两样就会变。
    if (
      merged.length === this.items.length &&
      merged.every((q, i) => {
        const old = this.items[i];
        return old.id === q.id && old.pending === q.pending;
      })
    ) {
      return;
    }
    this.items = merged;
  }

  /** 服务端确认这条排上了（不再有「还没听到回话」的悬念） */
  settle(id: string): void {
    this.items = this.items.map((q) =>
      q.id === id ? { ...q, pending: false } : q,
    );
  }

  remove(id: string): void {
    this.items = this.items.filter((q) => q.id !== id);
  }

  /** 一条会话没了：它排着的队也没地方发了 */
  dropSession(sid: string): void {
    this.items = this.items.filter((q) => q.sid !== sid);
  }

  /**
   * 撤掉一条**还在排队**的消息（虚线气泡左边那个 ×）。
   *
   * 队列在服务端，所以得告诉它 —— 不然那条照样会在下一个步边界交给模型。界面上
   * 立刻撤掉（不等回话）：真撤不掉（刚好在这一刻被交出去了）也没关系，下一次
   * `load` 就看到它已经落进正文了。
   */
  drop(id: string): void {
    const item = this.items.find((q) => q.id === id);
    this.remove(id);
    if (!item) return;
    void post("/api/sessions/unqueue", { session: item.sid, id }).catch(() => {
      /* 撤不掉就算了：它已经落进正文，或者下次轮询会纠正 */
    });
  }

  /**
   * 服务端把排队的消息交给模型了 → 把那几条从「排队中」挪进**正文**。
   *
   * 挪进去就成了本地历史的一部分（`done: true`）：服务端那边它们也确实是历史了
   * —— 紧接着流里会来一个 `start`，模型的回答进**新的一条**助手消息
   *（`consume` 的 `onRestart`），和服务端历史的形状一致（`session_view` 里 User
   * 会打断助手消息的合并）。
   *
   * 返回挪走的那几条（调用方往 `messages` 里推）。
   */
  deliver(ids: string[]): Queued[] {
    const moved = this.items.filter((q) => ids.includes(q.id));
    if (moved.length)
      this.items = this.items.filter((q) => !ids.includes(q.id));
    return moved;
  }
}
