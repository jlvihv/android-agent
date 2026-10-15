# 浏览器：agent 自己开一个 WebView

> 数据日期：2026-10-12
> 参考实现：OpenMinis（`~/Code/lib/OpenMinis` 的 `browser/`，GPL-3.0 —— 只参考机制，没有抄代码）

---

## 1. 为什么是 WebView，不是 HTTP 抓取

模型要能：看 JS 渲染出来的内容（SPA）、用登录态、点按钮、填表单。
`curl` 一个都做不到 —— 它拿到的是空的 `<div id="app">`。

所以是**一个真的 WebView**。它和用户手机上的 Chrome 是同一种东西：真渲染、
真 cookie、真网络栈、页面里的 JS 照跑。

## 2. 关键决策：**绝不和用户共用一个可见 WebView**

这一点定错了会让整个功能不可靠：

- **不能共用。** 如果 agent 用的是用户正看着的那个页面，用户随手一划/一点就把
  agent 的页面导走了；反过来 agent 跳转也会打断用户。
- **不需要可见。** WebView 不挂在任何窗口上也能加载页面、执行注入的 JS。
  所以 agent 有一个**自己的、无头的** WebView（`BrowserController`）。
- **不需要屏幕亮着。** 进程活着就行。（后台限制见第 6 节。）

以后如果用户想看 agent 在干什么，把这只 WebView **临时** `addView` 进一个面板
显示（看完摘出来），agent 照跑 —— 这是 OpenMinis 的做法，我们暂时没做。

## 3. 结构

```
                                     ┌─ Core::browser() ─ BrowserApi(uinffi) ─┐
POST /api/browser ───────────────────┤                                       ├─ BrowserController.kt
沙箱 `agent browser` ─(wget)─────────┘                                       │   （无头 WebView）
```

- **`BrowserHost`（uniffi callback）** 和 `DeviceHost`（无障碍）是**两条独立的路**。
  浏览器不需要无障碍权限，所以 `AgentApp.onCreate` 就注入了；设备那条要等服务连上。
- **`POST /api/browser`** 是唯一的入口，body 就是动作（一个 `action` + 可选参数）。
  ① `agent browser` 走它，于是 bash 能脚本化驱动浏览器；
  ② 走到一次工具调用要花一整轮 LLM，调试时不现实 —— 有个能直接打的入口，
  改一行 JS 就能立刻验。
- **模型只有一个门：沙箱里的 `agent browser`**（`core/src/cli.rs`）。
  动作是 `navigate / get_backbone / get_readable / get_text / click / type / press /
  find_elements / scroll / screenshot / set_user_agent / back / wait_for_dom_stable /
  js / new_tab / list_tabs / switch_tab / close_tab`。

  唯一入口是沙箱里的 `agent browser`（为什么不做成工具、以及截图不进上下文怎么补的，
  见 [docs/10](10-tools-vs-sandbox.md)）；CLI 对每个动作的参数个数上下限都校验
  （少给 / 多给都报错）。

  截图不在工具结果里：图落在 `/root/.browser/`，模型得自己 `read`。所以 `Core::browser()`
  在截图返回的 `text` 里写了一句「用 read 看它」（写进 JSON 字段而不是另打一行，
  免得 `| jq` / `| python3 -c json.load` 被打断）。模型本来就这么干（6 次截图
  全是自己 read 回来的）。

## 4. 无头 WebView 必须自己做的事

1. **给尺寸。** 不挂窗口的 View 永远不会被父容器测量，宽高是 0 ——
   视口 0×0 的页面懒加载不触发、移动版布局全错。所以在创建时
   `measure(EXACTLY w/h)` + `layout(0,0,w,h)`，用屏幕物理像素，
   于是页面看到的 `innerWidth` 就是 `屏幕宽 / density`（和用户自己开浏览器一致）。
2. **接 `onRenderProcessGone` 并返回 true。** 返回 false 的话 Android 会把
   整个 App 进程杀掉。返回 true 之后这只 WebView 已经废了，下次动作用新的。
3. **注入超时要能判定"这只 WebView 死了"。** 渲染进程卡死时
   `evaluateJavascript` 的回调**永远不来**；超时就标记坏掉、换一只，
   否则后面每条命令都往同一个死 tab 上撞。
4. **吞掉 alert/confirm/prompt。** 页面弹一个 `confirm()` 会把渲染进程卡住
   （而且我们用的是 Application context，根本没有窗口可弹）。吞掉、记下来
   下一轮告诉模型。
5. **只走 http(s)。** `intent://`、`tel:`、`file://` 一律挡掉 —— 页面不该能
   拉起别的 App，更不该读本地文件。全程**不用 `addJavascriptInterface`**：
   被访问的页面拿不到我们的任何能力。

## 5. 截图：两条路，先 GPU 后软件

**软件绘制**（`View.draw(软件 Canvas)`）不需要窗口、不需要屏幕亮着，DOM / 普通
图片 / 2D canvas 都在 —— 但 Chromium 收到软件绘制会在 CPU 上重新栅格化，而
**GPU 才有的层（WebGL、`<video>`）它拿不到**，结果是空白：同一个页面在
屏幕上有画面，截图里那块是背景色。

**GPU 合成**（`HeadlessCapture`）解决这个：临时给 WebView 搭一个 surface ——

```
ImageReader(RGBA) ──surface──> VirtualDisplay ──> Presentation 窗口 ──> 挂 WebView
        └──────────────────── 读回一帧 ◄── GPU 正常合成 ──┘
```

私有虚拟显示（`OWN_CONTENT_ONLY`，不需要权限、用户看不见）；窗口设
`NOT_FOCUSABLE | NOT_TOUCHABLE`（页面里的输入框绝不能把输入法弹到用户真屏幕上）。

**A/B**（模拟器，往页面里注入一块 WebGL 纯红画布）：

| 走哪条 | 画布区域的平均 RGB | 结果 |
|---|---|---|
| GPU 合成 | `254,0,0` | 红色看到了 ✓ |
| 软件绘制（面板开着，WebView 挂在真窗口上 → 只能走这条） | `21,21,21` | 就是背景色，WebGL 层不可见 |

耗时 149ms（建宿主 14ms、2 帧、安静）—— 比想象的便宜。

几个"不做就会踩"的点：

1. **`postVisualStateCallback` + 等安静**：Chromium 在 WebView detach 时会丢掉
   页面的栅格化 tile，重挂之后它先present 它手上有的（缺的地方是白的），
   然后陆续补齐。所以帧数够不够、以及"最近 150ms 没新帧"才是完整判据。
   只看帧数会拿到一张大半白页的图。
2. **RGBA 的 plane 行可能有 padding**（`rowStride > w*4`）：整块
   `copyPixelsFromBuffer` 会一行一行错位（画面斜着花），必须按行重排。
3. **ImageReader 的队列满了就不再回调**：必须一直 `acquireLatestImage` 丢掉旧帧。
4. **空闲 12s 释放宿主**：空闲的 App 不付渲染/内存/电量的钱，页面也不会在后台
   一直动画。尺寸变了就重建。
5. **连续失败 3 次就在本进程停用**，不让一台不支持虚拟显示的设备每次白试。
6. **面板正挂着这只 WebView 时不走这条路**（它本来就在真窗口上，挪走会让用户
   看着的那块闪白）→ 退回软件绘制。结果文本里会写明走的哪条（`GPU 合成` /
   `软件绘制`），排查"为什么 WebGL 是白的"时一眼能看出来。

**整页截图走不了这条路**：虚拟显示单边上限 4096px，而整页动辄 7000+ —— 这是
刻意的（不然要分配几万像素高的显示和位图）。所以整页截图里 WebGL 仍然是空白。

## 6. `get_readable`：读文章用这个，别用 `get_text`

`get_text` 给的是整个 body 的 innerText —— 导航、页脚、推荐位、cookie 声明全在
上面。一篇 CNBC 报道能拿回 **6597 字，一半是菜单和免责声明**。

`get_readable` 抽正文（Readability 的简化版，只留真管用的那几条）：

1. 候选容器：`article / main / [role=main] / div / section / td`。
2. 打分：里面的 `<p>`（≥ 25 字）每个算 `1 + min(字数/100, 3)`。
3. **正/负类名权重**：类名/id 命中 `article|body|content|story…` ×2，
   命中 `cookie|consent|privacy|nav|comment|promo…` ×0.15。同一份分数差十倍。
4. **包含页面 `h1` 的容器 +20** —— 正文容器几乎一定含着主标题。
5. 链接密度 > 50% 的块 ×0.3（那是导航/列表）。
6. 输出不取整个容器的 innerText，而是按文档顺序只取
   `h1-h3 / p / li / blockquote / pre / figcaption`，并跳过导航块；
   块里只有一个链接占主体时把地址附上（搜索页/目录页用）。

**两个坑**（cookie 声明当正文）：类名像周边的壳会把一堆正常 `<p>` 包在里面 —— 它段落又长又多，
分数比正文还高。所以两条缺一不可：① 周边判断要查**上面 6 层祖先**的类名
（cookie 的壳在很上面，`p` 自己的类名毫无异常）；② 前面那套权重。
同一页现在是 **4426 字的干净正文**（标题、导语、段落、无菜单无 cookie）。

## 7. 等页面安定：不要在页面里 `setTimeout`

`navigate` / `click` / `type` 之后都要“等页面安定”，否则读到的是半渲的动态内容。
判据是 **DOM 指纹连续 3 次一致**（元素个数 + 可见文字长度 + 滚动高度）。

关键：**在 App 侧轮询，不在页面里等**。不挂窗口的 WebView（大概率还切了后台）
会被 Chromium 把定时器节流到秒级，页面内轮询又慢又不准；App 侧注入是主动执行的。
代价是每个动作多几次往返（毫秒级）。

需要更久就显式调 `wait_for_dom_stable`（`timeout` 秒，默认 10、上限 30）。

## 8. selector 匹配多个要说话

`click` / `type` 用 `querySelectorAll(...)[0]`，但**结果里会写“匹配到 N 个，
点的是第一个”** —— 模型拿到这句话才知道自己可能点错了，该换更精确的 selector
还是用坐标。默默只点第一个，它就无从判断。

## 9. `get_backbone`：模型"看"页面的主要方式

不把 HTML 丢给模型（太大、全是噪音），抽一棵可见元素树：

1. 挑候选：可见（非 `display:none/visibility:hidden/opacity:0`、≥6px）、
   有内容（语义元素，或自己带文字且没有可见的元素子节点）。
2. **合并同位置的包装层**：祖先矩形和它几乎重合（±8px）就把代表提上去 ——
   现代页面上百个无意义 div 靠这步消失。**但碰见 `a/button/input` 就停**，
   否则 selector 指到 wrapper 上，点了没反应。
3. 内层外层文字一样时只留内层（三层包装一份标题 = 三倍噪音）。
4. 按面积建树、剪掉单孩子空壳、按 `max_depth` 截断、节点数封顶（220）。

**selector 必须唯一**，这是这个功能能不能用的分水岭：

- 从元素往上拼，每加一段就问一次"现在唯一了吗"，唯一就停。
- 数字开头的 id **不能**写成 `#50051583` —— 那是非法 CSS，`querySelector`
  直接抛 `SyntaxError`。要写成 `[id="50051583"]`。导航站的行 id 全是这种。
- 不能硬截成固定段数：HN 上每行的结构一样，截出来的 selector 全都一样，白忙一场。

锚点还会带上跨域域名（`-> www.cnbc.com /2026/…`），否则模型只看到
`/2026/…` 以为还在本站。

## 10. 操作能力：按键 / 找元素 / 切 UA / 整页截图

**`press`**（Enter / Tab / Escape / 方向键 / PageUp·Down / Home / End）。
两个必须自己补的事：

- **合成事件不会触发表单的隐式提交。** "在输入框里敲回车 → 提交"是 trusted
  event 的行为，`dispatchEvent` 出来的 keydown 不会引发它。所以 Enter 要显式
  `form.requestSubmit()`（没有就点 submit 按钮）。两件都做：很多现代站是自己
  在 keydown 里听 Enter，两条路各盖一半。
- **Tab 也不会移焦**，得自己算可聚焦元素列表再 `focus()`。

`keyCode` / `which` 在构造函数里给的值，部分 Chromium 版本不会真的落到属性上，
得用 `Object.defineProperty` 手动盖一层（页面上还活着的旧代码读这两个）。

**`find_elements`**：一个 selector 匹配到几个、各自长什么样。坐标给的是
**视口坐标**（`click` 的 `x/y` 要的正是它），同时给 `pageY` 让模型知道要滚多远。
backbone 太大、或者只想确认"到底有几个"时用它。

**`set_user_agent`**：`mobile` / `desktop`。**UA 和视口必须一起换** —— 只换 UA
的话，桌面版布局塞进 411px 宽，比移动版还乱。换完要重载（UA 是请求头，不重载
不生效）。默认那档直接用屏幕物理像素，不绕一圈 CSS px 再乘回去（两次取整会
少一个像素：411.4 → 410）。

**整页截图**（`screenshot full_page=true`）：`View.draw` 只画自己的矩形，所以把
View 临时拉高到全文档高度、画完再恢复。高度封顶 **8000px**（1080×8000 的 ARGB
位图已经 34MB，再高就是拿内存赌），截不全时结果里会写明。维基百科一篇
1080×7315 完整截下（含信息框），539KB。

## 11. 多标签

四个动作：`new_tab`（可带 url）、`list_tabs`、`switch_tab`、`close_tab`（不给
`tab` 就是关当前那只）。上限 4 —— 每只 WebView 都占渲染内存，而且模型同时盯
三页就已经转不过来（OpenMinis 那边也是 3 起步）。

**语义就一条：动作永远落在"当前标签"上。** 给了 `tab` 参数就先切过去再操作
（和点浏览器的标签一样），这样模型不用在脑子里记账 —— 它做完什么动作，
当时的页面就是它操作的那一只。每个动作的返回里都带 `tab` / `tabs`，
`list_tabs` 用 `→` 标出当前那只。

几个实现上必须做对的地方（都不是想当然能对的）：

1. **加载进度计数器必须挂在 tab 上，不能挂在控制器上。** 多标签会**同时**在
   加载，全局计数器会让一个后台标签的加载把前台的 `awaitLoad` 骗过去
   （看到计数变了就以为自己的页好了）。`dead` / `lastError` 同理。
2. **`active`（本次动作的目标 tab）在动作开始时定下来**，中途不跟着面板换 ——
   否则用户在面板上切一下标签，agent 的下一步就落到别的页上了。
3. **面板上的切换要 tryLock**：agent 正在跑动作时就忽略这次点击（那时页面是
   它在操作）。也因为它跑在主线程，`selectTabFromUi` **不能**用 `onMain` ——
   主线程里调 `onMain` 是"自己等自己" 10 秒然后抛异常。
4. **面板正挂着某只标签时把它关掉**：先从容器里摘下来再 `destroy`，然后
   把新当前标签挂上去（`panelHolder` 记着是哪个容器）。
5. **`list_tabs` / `new_tab` 不要顺手建标签**（`ensureWeb`）—— 模型问"有哪些
   标签"不该因此多出一只空标签（列一次就多一只）。

看板上：多于一只标签时头部下面出现一排胶囊（`1 baidu.com` / `2 127.0.0.1:33925`），
当前那只高亮，点它切过去。IP 不截断（截了看不出是哪儿），域名只留后两段。
标签条只在"标签集合 + 当前下标"变了才重建 —— 每 500ms 无条件重建会让用户
手指底下那颗按钮消失。

## 11. 看板：用户看 / 接管

agent 有一只自己的无头 WebView（第 2 节），而**同一个实例**要给人看时，就把它
临时 `addView` 进一块原生面板里；关掉面板再摘下来，agent 接着跑（页面不重载、
状态不丢）。**绝不另建一只** —— 否则用户看到的和 agent 操作的不是同一个页面。

**为什么是原生 View 而不是页面里的一块 HTML**：要显示的是另一只真 WebView，
而 WebView 不能嵌在网页里。所以这一层盖在 UI WebView 上面（`FrameLayout`
兄弟，平时 `GONE`，常态零开销）。实现在 `BrowserPanel.kt`。

**长相和手感照抄页面里的查看器**（`Viewer.svelte`）—— 同一个 App 里看沙箱网页
和看 agent 的浏览器是同一类事。所以：顶栏 36dp + 状态栏（`--hdr`）、
左边返回箭头（`Icon` 的 `back` 路径）+ 16sp 中粗标题（页面标题，没有就用 URL）、
agent 在动作时右边一行小字「agent 操作中」；内容区上沿一条分隔线；从右滑入
300ms quintOut，退场从当前位置接着滑；按住顶栏往右拖关闭（`lib/drag.ts`
同一套松手判定，拖了一半松手按 `RELEASE` 弹回）。和页面一样没有点按反馈。
这些数都是从 web 那边抄的，改那边就要改这边。

**标签条**：药丸样式（`rounded-full bg-control`），多了能横着滑。点了之后标签条
和标题**立刻**更新、只改高亮不重建（等下一拍轮询才重建整排的话，页面已经切过去了
标签条还停在原处，看着就是卡）。agent 正在动作时点击会被拒绝
（换掉当前标签它的下一步就落错页），会提示一句，不静默无反应。

两个入口：

- 头部的地球按钮（页面 → `AgentHost.openBrowser()` → 壳）
  > 坑：页面那边写的是 `host()?.openBrowser?.()`（桌面浏览器上壳不存在，
  > 要能跑）。所以壳里**忘了实现 `openBrowser()` 时不会报错** —— 按钮
  > 点下去静默无反应，看着像坏的。加/改 `AgentHost` 的方法时对着
  > `web/src/lib/overlay.ts` 的 `BackHost` 声明核一遍。
  >
  > **这个按钮只在 agent 真开着页面时才出现**（常驻着也没用：没页面时点它
  > 只会弹一句提示）。状态在壳这边（`AgentApp.browser.hasPage()`），所以
  > 两条腿：页面加载时同步拉一次（`AgentHost.hasBrowser()`），之后壳推
  > `window.__browserAvailable` —— `BrowserController.onPagesChanged`
  > 挂在 `setCurrent` 上（标签集合和 `web` 唯一的改动点），所以「开第一只
  > 标签 / 把标签关光」都会立刻反映到头部。
- `adb shell am start -n dev.vihv.agent/.MainActivity --ez browser true`
  —— 调试/验收用。看板是给人看的，但自己只能用手点，而**验收不该去戳用户
  正在用的手机屏幕**；能在 adb 里做的事就别靠摸屏幕（和 `files/port`、
  `agentctl` 一个思路）。

细节：

- **返回键**：面板开着时先关面板（页面的返回回调用 `PRIORITY_DEFAULT`，
  面板用 `PRIORITY_OVERLAY`，优先级天然更高）。**Android 14+ 跟手**
  （`OnBackAnimationCallback`）：手势进度直接当位移，和页面覆盖层一样；撤回
  弹回原位。API 33 只有松手关；33 以下走 `onBackPressed`。
- **安全区**：头让开状态栏、底部让开手势条。insets 三个来源依次回退
  （监听器 → 面板自己的 → Activity 的），因为面板 `GONE` 时 `rootWindowInsets`
  可能是 null。
- **一个必须知道的副作用**：面板把 WebView 重新布局成"面板里那么大"，
  页面高度变矮（2024 → 2021px，宽度不变）。所以打开面板会让页面重排，
  模型**按坐标**点可能对不上（按 selector 点不受影响）。选这个而不是给
  WebView 钉死尺寸：后者用户看不到视口底部，而且页面滚不动。
- 面板开着时 agent 照常操作（`get_readable` / 截图 / `js` 都验过），
  只是两人会抢同一个页面 —— 我们只显示状态，不锁用户的手。

## 12. 已知边界

- **动作是串行的**（一把锁）：两个动作交叉执行会让 `navigate A → click` 点到
  别的页面上。多标签是"多只页 + 当前那只"，不是"并行操作多只页"。
- **整页截图没有 GPU 层**（虚拟显示单边上限 4096px），视口截图有（见第 5 节）。
- **后台可能被回收 —— 已经摆平一半**：回合进行中会挂一个前台服务
（`docs/06`），所以“接个电话回来发现没跑完”不发生了：切到桌面后
浏览器动作照常跑完。但不能保证永远不被杀（用户手动强行停止、系统内存极端紧张），
所以动作仍尽量幂等，进度靠会话历史恢复。
- **吞掉的弹窗**只报"页面弹窗已自动关闭"，不做交互。

## 13. 打开沙箱里的网页（以及它们的存储）

AI 写了个静态网页，自己想看一眼：`agent browser navigate` 的 url 直接给沙箱路径 ——
`preview:/workspace/site/index.html`（和回复里给用户的链接同一个写法），也认
`preview:相对路径`、`file:///…` 和绝对路径 `/workspace/…`。`Core::browser` 把它换成
`http://sandbox.localhost:<端口>/sandbox/<钥匙>/workspace/site/index.html` 交给
WebView，结果里出现的这种 URL 再换回 `preview:/…`（端口每次启动可能不同、钥匙
不该进历史，模型记住稳定的写法）。`agent browser navigate preview:/…` 同样能用。

### 为什么是 `sandbox.localhost`，而不是和 App 同一个 `127.0.0.1`

沙箱文件名在**独立的源**上，不能和 App 同源：同源的话必须关进 opaque origin
（CSP `sandbox`、iframe 不给 `allow-same-origin`）—— 否则页面里的任何脚本都能带着
App 的凭据 cookie 调 `/api/linux`（沙箱挂着整个内部存储），代价是 **localStorage /
sessionStorage / IndexedDB 一碰就抛 SecurityError**，生成的网页一存东西就坏。

所以网页在 `sandbox.localhost` 上：

- `*.localhost` 由 Chromium 直接解析到 loopback（不走 DNS），而它和 `127.0.0.1`
  是不同的站点：App 的凭据 cookie（种在 `127.0.0.1`、`SameSite=Strict`）这个源上
  的页面读不到、请求也带不上。所以不需要 CSP sandbox，存储全能用。
- 这个源上**只有沙箱文件**（`split_origins` 中间件）：`/api`、`/auth`、App 页面
  一律 404。WebSocket 的 Origin 检查也不放行它。
- 同一个文件从 `127.0.0.1` 取照旧是 opaque origin —— 图片 / 音视频 / 文本这些 App
  自己取的还走同源（拉文本要读响应，跨源还得配 CORS）。
- 查看器的 iframe 指向 `sandbox.localhost` 并带 `allow-same-origin`（跨源才给；
  同源再给，页面就能拆掉自己的 sandbox）。聊天 WebView 打开了第三方 cookie ——
  对它来说这个 iframe 是第三方，关着的话 Chromium 连存储一起拦。
- **端口固定**（`AgentApp.PREFERRED_PORT`，被占才退回系统分配）：源带端口，端口每次
  启动都换的话，网页存的东西重启就没了。
- Android 的明文流量白名单对 `localhost` 开了 `includeSubdomains`。

已知差别：Chromium 按顶层站点给 iframe 的存储分区 —— 查看器里（顶层是 App）和
agent 浏览器里（顶层就是它自己）是两份存储，互不相通。各自都能持久。

## 14. 怎么调

不用花 token，也不用装 APK 之外的东西：

```sh
just phone                       # 交互模式
  /bash agent browser help                        # 用法
  /bash agent browser navigate https://example.com
  /bash agent browser get_backbone 2
  /bash agent browser get_text '#main'
  /bash agent browser get_readable                 # 文章页抽正文
  /bash agent browser wait_for_dom_stable 10       # 等动态内容安定
  /bash agent browser press Enter                  # 搜索框填完回车（会提交表单）
  /bash agent browser find_elements 'a[href]'      # 有几个、各自坐标
  /bash agent browser user_agent desktop           # 切桌面版（UA + 视口）
  /bash agent browser screenshot true              # 整页截图
  /bash agent browser click 'a[href="/next"]'
  /bash agent browser screenshot                  # 存 /root/.browser/
  /bash agent browser js 'return document.title'
```

排版复杂参数（x/y、多字段）直接给 JSON：
`agent browser '{"action":"click","x":100,"y":200}'`。

要拍照看结果：`cp /root/.browser/*.jpg /workspace/`，然后在文件管理器里看，
或者 `adb pull /sdcard/Agent/xxx.jpg`。

日志：`adb logcat -s AgentBrowser`（每个动作一行 `action=…`，包含参数）。

---

## 15. 桌面（Linux）：同一个 `BrowserApi`，换一个后端 = 无头 Chrome + CDP

手机上这个后端是壳里的 WebView（`BrowserController.kt`）。桌面上没有壳，
但 `BrowserApi` 本来就是 trait —— `core/src/cdp.rs` 是它的第三个实现
（前两个：Kotlin 的 WebView、测试里的 Echo/RecordingBrowser）。

**为什么是 CDP**（而不是嵌一个 WebView）：

| 方案 | 为什么不 |
|---|---|
| WebKitGTK | 要把 GTK/glib 链进 `.so` + GLib 主循环；而且它是 **WebKit**，安卓那边是 **Chromium** —— 注入的 JS 不保证同一种行为 |
| CEF / Electron / QtWebEngine | 几十上百 MB 的运行时 / 构建链，桌面开发不值 |
| **无头 Chrome + CDP** | 引擎和安卓同为 Chromium；`tungstenite` 早就因为 axum 在依赖树里了（**零新依赖**）；`scripts/dom-audit.ts` 已经在用 CDP 干真事 |

这也和既有取舍一致：LLM 协议自己写（不用 SDK），CDP 也自己写。

### Chrome 是**核心自己起的**

用的人不该先另开一个终端去起浏览器 —— 那是实现细节。第一次用到浏览器时核心
自己拉起一个无头 Chrome：**当场挑一个空闲端口**（不是写死 9222，同一台机器上
跑两个核心不会互相抢）、独立的 profile、日志落在 `<profile>/chrome.log`。

生命周期跟着核心：

- 正常退出（Ctrl-C / SIGTERM）→ `Drop` 里杀掉它；
- 被 `kill -9` / OOM 干掉（`Drop` 没机会跑）→ 下次启动时
  `sweep_orphan_chromes()` 收掉上次留下的（判据：命令行里有我们的 profile
  且**父进程已经是 init**，所以不会误杀还活着的兄弟核心起的那个）；
- profile 是持久的（`$XDG_DATA_HOME/android-agent/chrome`）—— 登录态留到下次；
  万一被另一个 Chrome 占着（SingletonLock），自动退一步用临时的。

要**用你自己那个浏览器**（能看着它动）或者接到别的机器上，就设
`AGENT_CDP=http://127.0.0.1:9222`（自己起一个带 `--remote-debugging-port` 的）；
`AGENT_CDP=off` 是不要浏览器。找不到 Chrome 时用 `AGENT_CHROME` 指可执行文件。

```sh
just dev-host                    # 或者 just dev —— 沙箱/宿主模式都能用它
agent browser navigate https://example.com
```

### 注入的 JS 只有**一份**

动作级的页面逻辑（骨架、正文、点击、输入、按键、滚动、找元素、DOM 指纹、
页面信息、`target=_blank` 修正）全在 `core/src/browser_js.rs`：

- 桌面：`cdp.rs` 直接调 `browser_js::build(kind, params)`
- 手机：Kotlin 通过 uniffi 的 `AgentCore.browserJs(kind, params)` 取
  （`BrowserController.kt` 里只剩「往哪只 WebView 里注、什么时候注、超时怎么办」
  这层宿主逻辑，~590 行 JS 从那边的文件里删掉了）

为什么不各写一份：动作表漂过一次（`new_tab` 把 URL 吃掉、模型对着白屏截了三张图），
而 JS 漂了更隐蔽 —— 点不动、抽不出正文，只有真机上才看得出来。

`kind` 一共 12 个（`browser_js::KINDS`），参数从 JSON 里取（两边都不用记顺序）。
动作**名字**那一层也有测试盯着：`cdp::ACTIONS` 和沙箱 CLI 的 case 表
（`cli::browser_actions_match_the_core`）。

**还留在 Kotlin 的**是宿主私事，不是共享逻辑：`js` 那个动作的「踢一脚 + 轮询」
（`evaluateJavascript` 不会等 Promise，而 CDP 有 `awaitPromise`）、以及截图那套
（`View.draw` / 虚拟显示，见第 5 节）里问页面要滚动位置的两小段。

### 和手机那份的其它差异

- **截图一条路**：`Page.captureScreenshot` 直接给 JPEG，不需要「GPU 合成 +
  软件绘制」那两条（第 5 节）—— 那是 Android 无窗口 WebView 的坑。
- **`js` 不用 kick + 轮询**：CDP 的 `awaitPromise` 会等 Promise。
- **截图落哪**：手机上是 `<rootfs>/root/.browser/`；宿主模式下 `/root` 是**用户的
  根目录**（写不进去也不该写），换成 `$XDG_DATA_HOME/android-agent/browser/`
  —— 前缀就是宿主路径本身，`read` 直接解得开。两边都由 `gc` 清扫
  （`gc::SideDirs`）。
- **URL 校验严一点**：`javascript:` / `data:` / `file:` 这些没有 `://` 的 scheme
  也挡掉（手机那份是「没有 `://` 就当主机名」，于是 `javascript:alert(1)` 会被
  拼成 `https://javascript:alert(1)`）；`host:8080/…` 靠「冒号后面是不是数字」放行。

真 Chrome 的端到端测试（自己起 Chrome，默认跳过 —— CI 上不一定装了）：

```sh
AGENT_CDP_TEST=1 cargo test cdp::          # 18 个动作全过一遍
```
