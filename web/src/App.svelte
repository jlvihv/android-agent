<script lang="ts">
  /**
   * 外壳。结构对齐 Claude 移动端：
   *   header（菜单 + 状态）/ main（空态标题 或 消息列表）/ 输入卡片
   *   抽屉 = Claude 侧边栏（导航项 + 最近对话 + 新建会话）
   *
   * 没有魔法数字：尺寸走 Tailwind 刻度，顶部/底部由安全区给。
   */
  import { untrack } from "svelte";
  import { store } from "./lib/store.svelte.ts";
  import { chat, MSG_WINDOW } from "./lib/chat.svelte.ts";
  import { SessionList } from "./lib/sessions.svelte.ts";
  import type { Message, Upload } from "./lib/sse";
  import { api, post } from "./lib/api";
  import {
    clearUrl,
    markCurrentEntry,
    onRouteChange,
    sessionFromPath,
    syncUrl,
  } from "./lib/route";

  import Icon from "./components/Icon.svelte";
  import Composer from "./components/Composer.svelte";
  import MessageList from "./components/MessageList.svelte";
  import Drawer, { type Session } from "./components/Drawer.svelte";
  import MachineSheet from "./components/MachineSheet.svelte";
  import ModelSheet from "./components/ModelSheet.svelte";
  import Settings from "./components/Settings.svelte";
  import Viewer from "./components/Viewer.svelte";
  import { viewer } from "./lib/viewer.svelte.ts";
  import { models } from "./lib/models.svelte.ts";
  import { agentHasBrowser, openAgentBrowser } from "./lib/overlay";
  import { wide } from "./lib/wide.svelte.ts";
  import { dismissable } from "./lib/drag";

  let input = $state("");

  /**
   * **眼前这条会话的活状态**（消息、窗口、流、用量）在 `lib/chat.svelte.ts` 里。
   *
   * 以前这些全是这个组件的 `$state` —— 于是「切走」只能等于「把流断掉」（旧流
   * 活着就会往新会话的列表里写，串内容），切回来只能靠 1.5 秒一次的轮询看进度，
   * 逐字流出来的东西全丢。现在流的所有者是模块，这个组件只是它的**读者**：
   * 切走不碰流，切回来接着看。
   *
   * `current` 为 `null` 只在「还没初始化」和「刚把这条删掉」这一瞬间 —— 模板里
   * 都按空态处理。
   */
  const current = $derived(chat.current);
  const messages = $derived(current?.messages ?? []);
  /** 有没有回合在跑（本地或远端）—— 输入框的停止键看它 */
  const running = $derived(current?.running ?? false);
  const usage = $derived(current?.usage ?? null);
  /** 服务端还有没有更早的（推导的：手上这窗的起点 > 0）*/
  const hasOlder = $derived(current?.hasOlder ?? false);
  /** 现在看的是哪条会话（`""` = 新对话 / 草稿）*/
  const sessionId = $derived(chat.viewing);

  // 圆环的分母：模型窗口。清单是启动时就拉好的（见下面的 models.load）
  let ctxWindow = $derived(models.contextWindow(store.llm?.model));

  let drawer = $state(false);
  /**
   * 宽屏下侧边栏要不要展开（窄屏不看它 —— 那里侧边栏是抽屉，看 `drawer`）。
   *
   * 默认**收起**：刷新之后永远是同一个样子（不会有“从关闭弹到打开”那一下），
   * 要用再点左上的菜单。不持久化 —— 这个 App 什么都不往 localStorage 里存。
   */
  let sidebar = $state(false);
  let sheet = $state(false);
  /** 「选择机器」面板（侧栏底栏那个机器标签点开的，见 MachineSheet） */
  let machineSheet = $state(false);
  /** 壳在不在（能在手机核心 / 电脑核心之间切）。桌面浏览器、手机浏览器里没有壳 */
  /**
   * 底栏那个机器标签点开「选择机器」（面板那份数据是本机核心答的，见 docs/05 的 4.5）。
   *
   * 这里不再看着有没有壳：电脑浏览器、手机 App、手机浏览器都问得到那份数据 ——
   * 以前只在 App 里是「页面上没有壳，换不了」，现在换机器整条路都在核心里（docs/12）。
   */
  const openMachines = () => (machineSheet = true);
  /** 设置页。可以从抽屉的齿轮进，也可以从模型面板「模型提供商」页的「管理模型提供商」进 ——
   *  后者是叠在模型面板上面的，关掉之后回到模型面板。每层各自在
   *  lib/overlay.ts 里登记，返回键先关最上面那层。 */
  let settings = $state(false);
  /** 设置页每关一次加一：Key 可能改了，模型面板据此重拉清单 */
  let settingsRev = $state(0);
  const closeSettings = () => {
    settings = false;
    settingsRev++;
  };

  /**
   * 开机就把模型清单拉回来。
   *
   * 「选择模型」面板是「第一帧就得有数据」才不跳（见 `lib/models.svelte.ts`）——
   * 等用户点开才拉的话，第一次打开还是会先渲染「载入中…」再长高。这里提前
   * 拉一次，之后 10 分钟内打开面板都是瞬间的，一个请求也不发。
   *
   * 没配服务商就不拉（`/api/models` 也没东西可回）。
   */
  $effect(() => {
    const on = !!store.llm;
    const r = settingsRev;
    if (!on) return;
    // `untrack`：同 ModelSheet 里的理由，别把 `load()` 内部读的 `$state` 也变成依赖
    void untrack(() => models.load(r));
  });

  /**
   * agent 有没有开着浏览器 —— 有才显示头部那个地球按钮。
   *
   * 状态在**壳**那边（`AgentApp.browser.hasPage()`，见 BrowserController）：
   * agent 调 `browser` 工具会懒建一只 WebView，把标签关光又回到「没有」。
   * 所以两条腿：挂载时同步拉一次（`AgentHost.hasBrowser`），之后壳推
   * `window.__browserAvailable`（见 MainActivity 的 `pushBrowserAvailable`）。
   */
  let browserAvailable = $state(false);
  $effect(() => {
    // 先挂上推送口，再拉一次初始值：同一个 tick 里不会漏掉中间那一下
    window.__browserAvailable = (on) => {
      browserAvailable = !!on;
    };
    browserAvailable = agentHasBrowser();
    return () => {
      delete window.__browserAvailable;
    };
  });

  /** 输入区实际高度。消息列表的底部留白和底部渐隐都跟着它走。 */
  let composerH = $state(0);

  // —— 会话 ——
  //
  // 消息、窗口、流、用量都在 `lib/chat.svelte.ts`（每条会话一份）。这个文件只
  // 剩下**编排**：路由、会话列表、抽屉、输入框。下面几个函数是给模板用的薄包装
  // ——`MessageList` 的 props 要的是函数，不是对象。

  /**
   * 翻页 / 预取 / 收窗口：都转给眼前这条会话。
   *
   * 以前这些各自带着「会话切走了没」的判断（`sid !== sessionId` 之类）；现在
   * 判断在 `Live` 自己身上（它认自己是哪条），这里只是转发。
   */
  const loadOlder = () => current?.page.loadOlder() ?? Promise.resolve(false);
  const prefetchOlder = () => current?.page.prefetchOlder();
  const trimOldPages = () => current?.page.trimOldPages();

  /**
   * 侧边栏那份会话列表：合并、游标分页、错误态全在 `lib/sessions.svelte.ts`。
   *
   * 以前这里是四个裸变量 + 一个 `slot` 占位对象，编码了「没在飞 / 在飞 / 刚落地 /
   * force 覆盖」四种状态 —— 状态机藏在变量组合里。现在是一个类，能单独测。
   */
  const list = new SessionList();
  const sessions = $derived(list.items);
  const loadSessions = (force = false) => list.load(force);
  const loadMoreSessions = () => void list.more();

  function newSession() {
    // **不请求服务端**：新会话到第一句话才建（`Chat#send` 里那段），这里只把
    // 草稿清空 —— 点了不发就不会攒出一堆空会话。
    chat.newDraft();
    // 新会话还没有 id：地址收回根路径（不带 id 的 `/` 就是「新对话」的意思）
    clearUrl();
    drawer = false;
  }

  /**
   * 切到另一条会话。
   *
   * 纯前端动作：改地址（`push`，后退能回上一条）→ 把「现在看这条」告诉 store。
   * 服务端没有任何「切换会话」的接口 —— 「用户在看哪条」归界面。
   *
   * **不断流**：正在跑的那条会话继续在后台收（服务端本来就不看有没有人听），
   * 切回来接着看，一个字都不少 —— 这是 `lib/chat.svelte.ts` 存在的全部理由。
   * 内存由那里的热集合兜着（超过三条就丢最久没看的那条）。
   *
   * **不重拉会话列表**：切一条会话并没有改变列表（标题、顺序、条数都不变）。
   * 列表该更新的时候（新建 / 删除 / 重命名 / 回合收尾 / 回前台）各自有自己的
   * 调用点。唯一会变的是「这条在不在跑」那个小圆点 —— 那个由轮询和 WS 的回合
   * 事件管，不该靠切会话去刷新。
   */
  function switchSession(id: string) {
    if (id === sessionId) {
      drawer = false;
      return;
    }
    syncUrl(id);
    chat.show(id);
    drawer = false;
  }

  async function renameSession(id: string, title: string) {
    try {
      await post("/api/sessions/rename", { id, title });
    } catch {
      /* 名字没改成就还是旧名字，下一次拉列表会纠正 */
    }
    await loadSessions(true);
  }

  /**
   * 删一条会话。
   *
   * **只有删的是当前正看着的那条，才清空视图**。以前清视图是无条件的 —— 在抽屉里
   * 长按删掉**别的**会话，正在看的那条会从界面上被抹掉，而地址栏还写着
   * `/s/<那条>`，刷新一下它又回来了。
   *
   * 删别的时候什么都不用动：列表重拉一次，当前会话原样留着。
   */
  async function deleteSession(id: string) {
    const isCurrent = id === sessionId;
    // 这条会话没了：消息、流、它排着的队一起丢掉（`forget` 里做）
    chat.forget(id);
    try {
      await post("/api/sessions/delete", { id });
    } catch {
      /* 删不掉的话下一次拉列表它还在，界面不会撒谎 */
    }
    if (isCurrent) {
      // 地址也收回根：`/s/<删掉的那条>` 是个死链接了
      clearUrl();
    }
    await Promise.all([loadSessions(true), current?.load()]);
  }

  // 只在挂载时跑一次。`untrack` 不能省：`chat.show()` 会顺手发一次 `load`，而它在
  // 第一个 await 之前同步读了 `busy` / `fresh` —— 不包的话它们都成了这个 effect
  // 的依赖，发第一句时 `fresh` 一变就多拉一次，和 `/api/chat` 赛跑。
  //
  // **首屏那一轮要记账**（`booted`）：下面还有三处会在挂载后紧接着触发同步
  // （WS 连上、`pageshow`、切回前台），它们各自都是「用户回来了 / 连接恢复了，
  // 补一次」，而首屏这一轮刚拉过 —— 不挡住就是同一份 190 KB 的历史立刻再发两遍。
  // 实测（Lighthouse 的 network-requests）：`/api/sessions` 发两次、
  // `/api/session` 一次在 37ms、一次在 1612ms，两次都把整窗历史拉回来。
  let bootedAt = 0;
  $effect(() =>
    untrack(() => {
      bootedAt = Date.now();
      // 地址栏里带了 `/s/<id>` 就认它（刷新 / 分享 / 从历史里回来都走这条）。
      //
      // **没带（`/`）= 空对话**：冷启动就该是「来思考些什么?」，不是把上次那条
      // 翻出来。「上次看的是哪条」这个东西服务端根本不存（见 core 的
      // `SessionStore` 注释），前端也不往 localStorage 里放 —— 这个 App 一直
      // 守着「不往本地存东西」那条线。
      // 先给首屏这条 history 打上我们的标记 —— 否则「从第一条会话后退回首屏」
      // 时那条 `state` 是 null，路由判据认不出来（见 `markCurrentEntry`）。
      markCurrentEntry();
      // `chat.show("")` = 空对话（`/` 就是「新对话」）。没带 id 时也要走一趟：
      // `viewing` 得有个确定的值，界面才不会停在「还没初始化」上。
      chat.show(sessionFromPath() ?? "");
      void loadSessions();
    }),
  );

  /**
   * 后退 / 前进 → 换会话。
   *
   * 只处理「地址变了」这一种：覆盖层的 popstate 由 `lib/overlay.ts` 自己接
   *（它看栈、不看地址）。两者压进 history 的顺序天然正确 —— 覆盖层是后压的，
   * 后退时它先弹出去，再轮到换会话。
   *
   * `id` 为 `null`（退回到 `/`）等于「新对话」：和点头部的「新对话」同一个语义
   *（`fresh` = 服务端还没有这条）。
   */
  $effect(() => {
    return onRouteChange((id) => {
      if (id === (sessionId || null)) return; // 地址没实质变化（覆盖层弹栈会走到这儿）
      if (!id) {
        // 退回到 `/` 只是「回到空对话」，**不是**「点了新对话」—— 草稿里写了一半
        // 的字得留着（清空是 `newSession` 的事，只有那个按钮该清）。
        chat.show("");
        drawer = false;
        return;
      }
      // 不重复 `syncUrl`：地址已经是它了，`switchSession` 里那次会自己认出没变
      void switchSession(id);
    });
  });

  /**
   * 「刚起来这一会儿不算数」的窗口（ms）。
   *
   * 首屏那几个同步入口（WS 连上 / `pageshow` / `visibilitychange`）和挂载几乎是
   * 同时到的；隔这么久之内的，认定为首屏那一轮已经覆盖，不再发。
   * 取 1500：够覆盖连接建立 + 首屏渲染，又不会把「用户真去后台转了一圈马上回来」
   * 这种挡掉（那本来就有各自的 `SYNC_GAP_MS` 兜着）。
   */
  const BOOT_GRACE_MS = 1500;
  const justBooted = () => Date.now() - bootedAt < BOOT_GRACE_MS;

  // 有人在**服务端**动这个会话 → 立即对一次账。
  //
  // 为什么必须有这一条：页面自己发起的回合它连着那条流（`live`），可别人发起的它
  // 一无所知 —— 手机上按的、另一台电脑上那个页面按的、排队消息接着开的那个回合，
  // 都是在别处动。这时页面闲着、`serverBusy` 还是 false，轮询根本没开，于是它一直
  // 停在旧内容上，直到人去切窗口（focus）/ 刷新。实测就是这样：服务端整段历史都写好
  // 了（`busy=false`），页面还是空态，`focus` 一响立刻全出来。
  //
  // 核心本来就把这件事放在总线上了（`Event::TurnActive` / `Event::Turn`，手机壳的
  // 通知栏就是听它），只是页面以前只处理 hello/device/log/ack，把这两个白收了。
  //
  // 这一次对账之后 `serverBusy` 就是 true，1.5 秒的轮询接着往下走；收尾那一下（false）
  // 也补一次，把最后一步的正文捞回来（轮询可能在最后一条落进会话之前就停了）。
  $effect(() => {
    // 只为了盯住这个位：回合起 / 止各变一次，一变就把**所有热会话**对一遍
    //（以前只有眼前这条 —— 别的会话在跑时界面完全不知道，见上面那段）。
    void store.turnActive;
    void untrack(() => chat.syncAll());
  });

  // WS 重新连上 → 也同步一次。
  //
  // 连接可能断很久（换网络、核心重启、WebView 刚从系统冻住里醒来），这期间服务端
  // 发生的事这条页面一概不知。`reloadHealth` 在 store 里替我们做了（模型 / 能力 /
  // 沙箱钥匙），消息列表只能这里补。
  $effect(() => {
    if (!store.connected) return;
    void untrack(() => {
      // 首屏那一轮刚拉过就别再来一遍（这条在挂载后几毫秒就会响）
      if (justBooted()) return;
      chat.syncAll();
    });
  });

  // 切回前台 / 重新聚焦 → 同步一次。
  //
  // 为什么需要：App 切到后台时 WebView 会挂起 JS，SSE 流可能就断了。
  // 但**服务端的 agent 不会停**（前台服务保着进程，Rust 线程照跑，
  // SSE 客户端断开只是让 tx.send 失败然后被忽略）。
  // 所以切回来时从服务端拉最新状态，而不是相信本地的旧视图。
  //
  // 三个事件常常一起来（手机切回前台：visibilitychange + focus + pageshow），
  // 只认第一个：不然同样的两个请求一下发三遍 —— 经隧道 / 中继时每个都是一条新连接。
  /** 两次同步至少隔这么久（ms） */
  const SYNC_GAP_MS = 1000;
  let lastSync = 0;
  $effect(() => {
    const sync = () => {
      if (document.visibilityState !== "visible") return;
      // `pageshow` 在**首次加载**时也会触发（不只是从 bfcache 恢复），而
      // `lastSync` 初值是 0 —— 以前这条等于「挂载后立刻再拉一轮」。首屏那轮
      // 刚拉过就跳过。
      if (justBooted()) return;
      const now = Date.now();
      if (now - lastSync < SYNC_GAP_MS) return;
      lastSync = now;
      chat.syncAll();
      void loadSessions();
    };
    document.addEventListener("visibilitychange", sync);
    window.addEventListener("focus", sync);
    window.addEventListener("pageshow", sync);
    return () => {
      document.removeEventListener("visibilitychange", sync);
      window.removeEventListener("focus", sync);
      window.removeEventListener("pageshow", sync);
    };
  });

  // 服务端还在跑 → 轮询到结束。**这个 effect 搬去 `Chat#syncPoll` 了**：
  // 现在可能同时有几条会话在远端跑（热集合），谁在跑就轮到谁 —— 盯一个
  // `running`（只看当前会话）会漏掉别的。轮询本身留着，它是 WebView 被冻住 /
  // 流真的断了时唯一的兜底（见 docs/06）。

  // 排队那条「交出去了」的处理（`data-queued-delivered` → 挪进正文）在
  // `Chat#deliverQueued` 里 —— 它得知道是哪条会话的流，以前这里只有一个没有
  // sid 的全局回调（两条会话同时跑就会把 A 的排队项挪进 B）。

  // 当前会话里排队的那几条（其它会话的排着队，切回去才看得见）。
  const queued = $derived(chat.queuedOf(sessionId));

  // 它们按「**还没发出去**的用户消息」渲染（`done: false`）—— 消息列表里就是
  // 普通用户气泡的样子，只多一圈虚线和左边一个 ×（详见 `Queued` 的注释）。
  const queuedMsgs = $derived(
    queued.map(
      (q): Message => ({ id: q.id, role: "user", parts: q.parts, done: false }),
    ),
  );

  // 左上角那个菜单按钮：窄屏是开抽屉；宽屏是**收 / 展**侧边栏
  // （宽屏侧边栏一直在，只是可以收起来把地方让给对话）。
  //
  // **打开不发请求**。抽屉里只有三样东西：会话列表 / 收藏 / 机器标签，前两样都是
  // 模块级状态（`sessions` / `favorites`），不需要「打开时去确认一下」：
  //
  //   - 本页改过列表（新建 / 重命名 / 删除 / 切会话 / 回合收尾）→ 那些地方自己已经拉了；
  //   - 用户从别处回来（切前台 / 聚焦 / 连接恢复）→ 那三条入口已经拉了；
  //   - 首屏 → 挂载时拉了。
  //
  // 以前这里会多发一份 `/api/sessions`（22 KB，手机 WiFi 上几百毫秒），为的是盖住
  // 「别的客户端在服务端改了、本页一直在前台没动」这一种情况 —— 那一下的代价和它
  // 出现的频率不成比例，而真碰上了切一下前台就是最新的。
  function toggleSidebar() {
    if (wide.on) sidebar = !sidebar;
    else drawer = true;
  }

  // 返回键关覆盖层：每个覆盖层自己登记（lib/overlay.ts），这里不管。
  const close = () => {
    drawer = false;
    sheet = false;
  };

  // —— 在对话区往右滑，打开抽屉（窄屏）——
  //
  // 跟手：拖多少抽屉就出来多少，松手看速度 / 位置决定展开还是收回 —— 和往左拖关抽屉
  // 是同一套手势（lib/drag.ts），只是方向反过来、拖的是还没出来的抽屉。
  //
  // 从屏幕**最左边**起手的那一下归系统（Android 的返回手势），轮不到页面；从对话中间
  // 往右划就行。

  /** 拖着打开时抽屉拉出来的比例（交给 Drawer 的 `peek`）；null = 不在跟手 */
  let peek = $state<number | null>(null);
  /** 触屏才开：桌面上用鼠标往右拖是在选字，不是想开抽屉 */
  const touch =
    typeof matchMedia === "function" && matchMedia("(pointer: coarse)").matches;

  /** 这一下能不能当成「开抽屉」：输入框里是拖光标；能横着滚、又没在最左的内容（代码块、
   *  宽表格）是想把它往回滚 */
  function swipeable(t: EventTarget | null): boolean {
    for (
      let el = t instanceof Element ? t : null;
      el && el !== document.body;
      el = el.parentElement
    ) {
      if (el.matches('input, textarea, [contenteditable="true"]')) return false;
      if (el.scrollLeft > 0) return false;
    }
    return true;
  }

  const openDrag = {
    axis: "x" as const,
    dir: 1 as const,
    // 抽屉宽 92%（app.css 的 --drawer-w）：手指走完这么远就是整个拉出来了
    size: () => innerWidth * 0.92,
    canStart: (t: EventTarget | null) =>
      touch && !wide.on && !drawer && swipeable(t),
    onStart: () => {
      peek = 0;
      drawer = true;
    },
    onMove: (p: number) => (peek = p),
    onEnd: (open: boolean) => {
      if (open) peek = null;
      else drawer = false; // 出场动画从当前位置接着收（Drawer 的 slideOut）
    },
  };
  // 收回之后把跟手状态清掉，下次点菜单打开就是正常的进场动画
  $effect(() => {
    if (!drawer) peek = null;
  });

  // —— 对话 ——
  //
  // 发消息 / 收流 / 停止全在 `lib/chat.svelte.ts`（`Chat#send`、`Live#deliver`、
  // `Live#stop`）。这里只剩三件界面上的事：清输入框、把视口带回尾部、刷新抽屉。

  /**
   * 发一条消息（发到界面正看着的那条会话）。
   *
   * 空消息不发（只有附件也算有内容）。`ownSend` 在 `Chat#send` 里加 —— 那才是
   * 「用户自己发的」，服务端交付排队消息不算（用户可能正往上翻着看历史）。
   */
  function send(attachments: Upload[]) {
    const text = input.trim();
    if (!text && attachments.length === 0) return;
    input = "";
    void chat.send(text, attachments);
  }

  /** 撤掉一条还在排队的消息（虚线气泡左边那个 ×）*/
  const dropQueued = (id: string) => chat.dropQueued(id);

  /** 停止眼前这个会话的回合 */
  const stop = () => current?.stop();

  /**
   * **新会话出现在侧边栏** —— 在「第一帧到了」这个时刻刷新列表，不是在建会话时。
   *
   * 为什么要挑这个时机：`POST /api/sessions` 建出来的会话**还没有内容**，而
   * `/api/sessions` 的 `list_page` 按 `has_content()` 过滤 —— 那一刻重拉是拿不到
   * 它的（实测：点新对话、发第一句，抽屉里一直是空的，要等整个回合跑完才冒出来）。
   * 第一帧到达时 `run_turn` 已经把用户消息和标题写进历史了，这时它才真的在列表里。
   *
   * `force`：绕开 300ms 的合并窗口 —— 这个时机只有一次，晚到就是「侧边栏没有新
   * 会话」。
   */
  $effect(() =>
    chat.on("started", () => {
      void loadSessions(true);
    }),
  );

  /**
   * 一条流收尾 → 再刷一次列表（标题定下来了、顺序也可能变，那个闪烁的点要落）。
   *
   * 以前这行在 `run()` 的 `finally` 里；现在流不归这个组件管了，用事件接上。
   * 别的会话在后台收完也算。
   */
  $effect(() =>
    chat.on("ended", () => {
      void loadSessions(true);
    }),
  );

  /**
   * 草稿在服务端建出来了 → 地址栏换成 `/s/<id>`。
   *
   * `replace`（不是 `push`）：用户点「新对话」那一下不该在历史里留一格「空对话」，
   * 否则从新会话按一次后退会退到另一个空对话，看着像没反应。
   *
   * **这里不刷列表**：这会儿服务端那条会话还没内容，`/api/sessions` 会把它过滤掉
   * （见上面 `started` 那段）。
   */
  $effect(() =>
    chat.on("created", (id) => {
      syncUrl(id, true);
    }),
  );
</script>

<!-- 抽屉那堆 props 只有「并排 / 盖在上面」这一处不同，两个宽度下各挂一次。
     以前是把同一份 11 个 prop 抄了两遍，加一个 prop 要记得改两处。 -->
{#snippet drawerBody(pinned: boolean)}
  <Drawer
    {sessions}
    {peek}
    onclose={close}
    onswitch={switchSession}
    ondelete={deleteSession}
    onrename={renameSession}
    onsettings={() => (settings = true)}
    machine={store.machine}
    onmachine={openMachines}
    onmore={loadMoreSessions}
    more={list.next !== null}
    moreBusy={list.moreBusy}
    moreError={list.moreError}
    current={sessionId}
    {pinned}
  />
{/snippet}

<div class="flex h-full overflow-hidden bg-ink">
  <!-- 宽屏：侧边栏**并排**在左边（不是抽屉），默认收着，点左上角的菜单展开。
       窄屏：它还是盖在对话上的抽屉，同样是那个菜单按钮控制（见下面）。 -->
  {#if wide.on && sidebar}
    {@render drawerBody(true)}
  {/if}

  <!-- 对话区：把侧边栏剩下的宽度**全部**拿走（不限宽）。 -->
  <div
    class="shell relative h-full w-full min-w-0 flex-1 overflow-hidden"
    style:--composer-h="{composerH}px"
    use:dismissable={openDrag}
  >
    <!-- 消息列表铺满整个高度：内容从头部和输入卡片下面穿过去。
         顶部/底部的渐隐是 `.shell` 的伪元素（只为了装饰，不值得占两个 div）。 -->
    <!-- 不要给 main 加 z-index：`z-0` 会形成层叠上下文，把里面
         弹出面板的 z-50 困住，压不过同级 z-20 的输入框（踩过）。
         z-auto 的定位元素不建层叠上下文，浮层的 z-10 依旧在它上面。 -->
    <main class="absolute inset-0">
      {#if messages.length === 0 && queuedMsgs.length === 0}
        <h1 class="grid h-full place-items-center text-2xl font-normal">
          来思考些什么?
        </h1>
      {:else}
        <MessageList
          {messages}
          pending={queuedMsgs}
          ondrop={dropQueued}
          follow={current?.ownSend ?? 0}
          session={sessionId}
          hasolder={hasOlder}
          onloadearlier={loadOlder}
          onprefetch={prefetchOlder}
          ontrim={trimOldPages}
          step={MSG_WINDOW}
        />
      {/if}
    </main>

    <!--
      渐隐用浮层而不是滚动容器上的 mask。
      mask 会让滚动容器每帧重新合成 —— 实测手机上划动明显卡顿。
    -->

    <!-- 头部浮在内容上 -->
    <header
      class="pointer-events-none absolute inset-x-0 top-0 z-20 flex items-center justify-between
             px-4 pb-1 pt-[calc(0.25rem+env(safe-area-inset-top))]"
    >
      <!-- 窄屏：开抽屉；宽屏：收 / 展侧边栏（两个宽度下都在） -->
      <button
        onclick={toggleSidebar}
        aria-label="菜单"
        title={wide.on ? "收起 / 展开侧边栏" : "菜单"}
        class="tap-y pointer-events-auto p-1 text-fg"
      >
        <Icon name="menu" size={20} />
      </button>
      <!-- 右上角 = 新对话（对齐 Claude 移动端：左边菜单，右边开新会话）。 -->
      <div class="pointer-events-auto ml-auto flex items-center gap-1">
        <!-- agent 的浏览器：它自己在后台开了网页时，用户可以看看它在干什么、
             也能自己接着操作（壳里那块是**同一只** WebView）。
             **只在它真开着页面时才出现** —— 没开过页面时点它只会弹一句
             「还没打开过网页」，那就不该显示（也少一个常驻图标）。 -->
        {#if browserAvailable}
          <button
            onclick={openAgentBrowser}
            aria-label="看 agent 的浏览器"
            title="看 agent 的浏览器"
            class="tap-y p-1 text-fg"
          >
            <Icon name="globe" size={20} />
          </button>
        {/if}
        <button
          onclick={newSession}
          aria-label="新对话"
          title="新对话"
          class="tap-y p-1 text-fg"
        >
          <Icon name="newchat" size={20} />
        </button>
      </div>
    </header>

    <!-- 输入卡片也浮在内容上 -->
    <div class="absolute inset-x-0 bottom-0 z-20" bind:offsetHeight={composerH}>
      <!-- 排队中那几条不在这里（输入框上方）—— 它们画在**消息列表末尾**，
           和已发送的用户气泡同一个样子（虚线 + 左边一个 ×），见 MessageList。 -->

      <Composer
        bind:value={input}
        busy={running}
        {usage}
        {ctxWindow}
        onsend={send}
        onstop={stop}
        onmodel={() => (sheet = true)}
      />
    </div>
  </div>

  <!-- 窄屏：抽屉盖在对话上（宽屏它已经并排在左边了） -->
  {#if !wide.on && drawer}
    {@render drawerBody(false)}
  {/if}
</div>

{#if sheet}<ModelSheet
    onclose={close}
    onconfig={() => (settings = true)}
    rev={settingsRev}
  />{/if}

{#if machineSheet}<MachineSheet onclose={() => (machineSheet = false)} />{/if}

{#if settings}<Settings onclose={closeSettings} />{/if}

{#if viewer.url}<Viewer />{/if}
