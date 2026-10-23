# 应用图标的源文件

这里放 SVG 源 —— `res/` 不认 SVG，所以 PNG 是手动渲染出来的。

| 文件                         | 对应                                                                                                               |
| ---------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `ic_launcher_foreground.svg` | 自适应图标的**前景层**。矢量版在 `res/drawable/ic_launcher_foreground.xml`（手写的 path）—— **改图形时两边都要改** |
| `ic_launcher.svg`            | 传统方形图标（PNG 兜底 + 商店截图用）                                                                              |
| `ic_launcher_round.svg`      | 传统圆形图标                                                                                                       |

重新渲染 PNG（`rsvg-convert` 来自 librsvg）：

```sh
cd android/app
for spec in "mdpi 48" "hdpi 72" "xhdpi 96" "xxhdpi 144" "xxxhdpi 192"; do
  set -- $spec
  rsvg-convert -w $2 -h $2 icon-src/ic_launcher.svg       -o src/main/res/mipmap-$1/ic_launcher.png
  rsvg-convert -w $2 -h $2 icon-src/ic_launcher_round.svg -o src/main/res/mipmap-$1/ic_launcher_round.png
done
```

## 设计约束（改之前先看）

- **内容要落在中心 r=36 的圆内**（108×108 画布）。自适应图标外圈 18dp 会被启动器
  裁掉，而 Pixel 默认是**圆形**遮罩 —— 只满足「72×72 安全区」不够，圆角方形的四个角
  照样被切。所以机器人是照内切圆摆的，不是照安全区。
- 颜色只用 App 自己的令牌（`web/src/app.css`）：脑袋 `fg #f9f9f7`、眼睛和底色
  `ink #151515`、天线那颗点 `accent #6da7ec`。
- 单色层（`res/drawable/ic_launcher_monochrome.xml`，给 Android 13+ 的主题图标）里，
  眼睛必须用 `evenOdd` 挖成**洞**，不能画成深色圆点 —— 主题图标会把整层染成一个颜色，
  深色圆点会被一起染掉，眼睛就消失了。
- **48px 是最小实用尺寸**（启动器列表、通知）。改完缩到 48 看一眼再提交 —— 天线那颗
  点和眼睛的间距都是照这个尺寸留的。
- 系统开了「主题图标」（壁纸取色）时，桌面上的图标会被染成单色 —— 用的是上面那个
  `monochrome` 层。应用信息页里看到的才是本色（深底 + 暖白脑袋 + **蓝点天线**）。
