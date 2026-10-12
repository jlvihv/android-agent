# 浏览器：agent 自己开一个 WebView

> 实测日期：2026-10-12
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
Rust 工具 browser ─┐
                   ├─ Core::browser() ─ BrowserApi(uinffi) ─┐
POST /api/browser ─┘                                        ├─ BrowserController.kt
沙箱 `agent browser` ──(wget /api/browser)──────────────────┘   （无头 WebView）
```

- **`BrowserHost`（uniffi callback）** 和 `DeviceHost`（无障碍）是**两条独立的路**。
  浏览器不需要无障碍权限，所以 `AgentApp.onCreate` 就注入了；设备那条要等服务连上。
- **`POST /api/browser`** 直接跑一个动作（body 就是工具参数的形状）。
  存在的理由：① `agent browser` 走它，于是 bash 能脚本化驱动浏览器；
  ② 走到一次工具调用要花一整轮 LLM，调试时不现实 —— 有个能直接打的入口，
  改一行 JS 就能立刻验。
- **`browser` 工具**：`action` 枚举 + 可选参数。动作是
  `navigate / get_backbone / get_readable / get_text / click / type / scroll / screenshot / back / wait_for_dom_stable / js`。
  描述里写清"什么时候用它、什么时候 curl 更快"，这决定模型会不会用对。

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

## 5. 截图：软件绘制，但有边界

`View.draw(Canvas)` 不需要窗口、不需要屏幕亮着。两个坑：

- **滚动补偿。** `View.draw` 不做父容器的 translate，不补的话内容会往下偏一个
  scrollY（顶部空白、底部被裁）。而且滚动位置在**合成器**里 —— 不挂窗口时
  `webView.scrollY` 恒为 0，只能问页面要（`window.scrollY`）。
- **GPU 层是空的。** Chromium 收到软件绘制会在 CPU 上重新栅格化：DOM、普通图片、
  2D canvas 都在，**WebGL / `<video>` 会是空白**。真需要的话得临时搭一个私有
  虚拟显示（`VirtualDisplay` + `ImageReader` + `Presentation`），OpenMinis 那份
  有 300 多行、还有一堆 OEM 兼容问题 —— 等真挡住事了再做。

截图落在沙箱的 `/root/.browser/`，Rust 侧再走 `read` 那条图片管线
（解码 → 缩到 2000 → 重编码 → 落盘）挂进 tool 结果，所以模型看到的是一张
压过的图，历史里存的也是压过的那份。**截图不会自动附**：它进历史就破坏
prompt cache（见 03），想看画面要显式调 `screenshot`。

## 6. `get_readable`：读文章用这个，别用 `get_text`

`get_text` 给的是整个 body 的 innerText —— 导航、页脚、推荐位、cookie 声明全在
上面。实测一篇 CNBC 报道拿回 **6597 字，一半是菜单和免责声明**。

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

**踩过的坑**：第一版把 CNBC 的 cookie 声明当成了正文 —— 它段落又长又多，
分数比正文还高。两个修正缺一不可：① 周边判断要查**上面 6 层祖先**的类名
（cookie 的壳在很上面，`p` 自己的类名毫无异常）；② 前面那套权重。
修完同一页：**4426 字的干净正文**（标题、导语、段落、无菜单无 cookie）。

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
- 早先版本硬截成 3 段，于是 HN 上每个链接的 selector 全都一样，白忙一场。

锚点还会带上跨域域名（`-> www.cnbc.com /2026/…`），否则模型只看到
`/2026/…` 以为还在本站。

## 10. 已知边界

- **单标签**。一个进程一只 WebView、一把锁串行化动作（两个动作交叉执行会让
  `navigate A → click` 点到别的页面上）。多标签要引入池 + 归属（OpenMinis 那套）。
- **截图没有 GPU 层**（见第 5 节）。
- **后台可能被回收 —— 已经摆平一半**：回合进行中会挂一个前台服务
（`docs/06`），所以“接个电话回来发现没跑完”已经不发生了 —— 实测切到桌面后
浏览器动作照常跑完。但不能保证永远不被杀（用户手动强行停止、系统内存极端紧张），
所以动作仍尽量幂等，进度靠会话历史恢复。
- **吞掉的弹窗**只报"页面弹窗已自动关闭"，不做交互。

## 11. 怎么调

不用花 token，也不用装 APK 之外的东西：

```sh
just phone                       # 交互模式
  /bash agent browser help                        # 用法
  /bash agent browser navigate https://example.com
  /bash agent browser get_backbone 2
  /bash agent browser get_text '#main'
  /bash agent browser get_readable                 # 文章页抽正文
  /bash agent browser wait_for_dom_stable 10       # 等动态内容安定
  /bash agent browser click 'a[href="/next"]'
  /bash agent browser screenshot                  # 存 /root/.browser/
  /bash agent browser js 'return document.title'
```

排版复杂参数（x/y、多字段）直接给 JSON：
`agent browser '{"action":"click","x":100,"y":200}'`。

要拍照看结果：`cp /root/.browser/*.jpg /workspace/`，然后在文件管理器里看，
或者 `adb pull /sdcard/Agent/xxx.jpg`。

日志：`adb logcat -s AgentBrowser`（每个动作一行 `action=…`，包含参数）。
