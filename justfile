# android-agent 构建编排
#
# 刻意**不让 Gradle 感知 Rust**：Gradle 只负责把 jniLibs/ 和 assets/ 打进 APK，
# cargo 由这里驱动。理由见 docs/01-tech-selection.md 的 D8。

set shell := ["bash", "-uc"]

# 没有这行的话，带 `*ARGS` 的 recipe 里 `"$@"` 是空的 —— just 默认只做
# `{{...}}` 插值，不把参数传给 shell 的位置参数。踩过一次：`just phone --json "…"`
# 静默地什么都没发出去（agentctl 收到空参数 → 进交互模式 → stdin 是管道 → 直接退出）。
set positional-arguments

core  := "core"
web   := "web"
and   := "android"
# 出哪些 ABI。默认只出 arm64 —— 真机上白带一份 x86_64 是 ~4MB。
# 模拟器要 x86_64：`AGENT_ABIS=arm64-v8a,x86_64 just emu-install`
abis  := env_var_or_default("AGENT_ABIS", "arm64-v8a")
mod   := and / "app"
jni   := mod / "src/main/jniLibs"
assets := mod / "src/main/assets"

# NDK：从 SDK 里挑版本号最高的那个，避免依赖环境变量
# SDK：Linux 上习惯放 ~/Android/Sdk，macOS 上 Android Studio 默认放 ~/Library/Android/sdk
sdk_home := env_var_or_default("ANDROID_HOME", `for d in "$HOME/Android/Sdk" "$HOME/Library/Android/sdk"; do [ -d "$d" ] && { echo "$d"; exit; }; done`)
export ANDROID_HOME := sdk_home
ndk_home := env_var_or_default("AGENT_NDK_HOME", `ls -d "${ANDROID_HOME:-$HOME/Android/Sdk}"/ndk/*/ "$HOME"/Android/Sdk/ndk/*/ "$HOME"/Library/Android/sdk/ndk/*/ 2>/dev/null | sort -V | tail -1`)
export ANDROID_NDK_HOME := ndk_home

# JAVA_HOME 不在这里设。
#
# 用户环境里经常有个坏掉的 JAVA_HOME（比如指向没装的 Android Studio），
# 直接信任它会让 Gradle 起不来。所以下面每个用到 Gradle 的 recipe
# 自己探测一个能用的。

# --- Rust ------------------------------------------------------------------

check:
    cargo clippy --manifest-path {{core}}/Cargo.toml --all-targets -- -D warnings

test:
    cargo test --manifest-path {{core}}/Cargo.toml

# 起核心：mock 设备 + 本地 Linux 沙箱（桌面）
#
# 需要先 `just web-build`（rust-embed 在编译期嵌入 web/dist）。
dev:
    #!/usr/bin/env bash
    set -a; [ -f .env.local ] && . ./.env.local; set +a
    export AGENT_LLM_API_KEY="${AGENT_LLM_API_KEY:-${OPENCODE_API_KEY:-}}"
    export AGENT_LLM_BASE_URL="${AGENT_LLM_BASE_URL:-${OPENCODE_BASE_URL:-}}"
    export AGENT_LLM_MODEL="${AGENT_LLM_MODEL:-${OPENCODE_MODEL:-}}"
    export AGENT_LLM_NAME="${AGENT_LLM_NAME:-opencode-go}"
    export AGENT_LINUX_ROOTFS="${AGENT_LINUX_ROOTFS:-$PWD/.dev/linux/rootfs}"
    export AGENT_LINUX_PROOT="${AGENT_LINUX_PROOT:-$(command -v proot || echo proot)}"
    cargo run --manifest-path {{core}}/Cargo.toml --bin agent-dev

# 起核心：**宿主模式**（桌面原生，没有沙箱）
#
# 和 `just dev` 同一份东西，区别只在 `bash` 跑在哪：这里是**这台机器本身**——
# `read` / `edit` 改的是真文件，`bash` 用的是你的 PATH / 你的工具。
#
# cwd（＝ /workspace 的落点）默认是 justfile 所在的目录（`just` 会把 cwd
# 设到这里），也就是这个仓库。换成别处：
#
#   AGENT_HOST_WORKSPACE="$PWD" just dev-host     # 你当前那个目录
#   AGENT_HOST_WORKSPACE=~/work just dev-host      # 或直接给绝对路径
#
# 模型的改动是**真的**，没有回收站；要原地验证大改动就用 `just dev`（沙箱）。
dev-host:
    #!/usr/bin/env bash
    set -a; [ -f .env.local ] && . ./.env.local; set +a
    export AGENT_LLM_API_KEY="${AGENT_LLM_API_KEY:-${OPENCODE_API_KEY:-}}"
    export AGENT_LLM_BASE_URL="${AGENT_LLM_BASE_URL:-${OPENCODE_BASE_URL:-}}"
    export AGENT_LLM_MODEL="${AGENT_LLM_MODEL:-${OPENCODE_MODEL:-}}"
    export AGENT_LLM_NAME="${AGENT_LLM_NAME:-opencode-go}"
    export AGENT_LINUX_HOST=1
    export AGENT_HOST_WORKSPACE="${AGENT_HOST_WORKSPACE:-$PWD}"
    cargo run --manifest-path {{core}}/Cargo.toml --bin agent-dev

# 起一个**无头 Chrome**，开 CDP 调试端口 —— 桌面上 agent 的浏览器就是它。
#
# 为什么不由核心自己起：端口、profile、退出清理都是麻烦，而且你可能想让 agent
# 用**你自己**那个浏览器（`google-chrome --remote-debugging-port=9222`，还能看着
# 它动）。核心只负责连 `AGENT_CDP`（默认 127.0.0.1:9222），连不上时报的错里
# 会写怎么办。
#
#   just chrome                       # 无头，独立 profile（.dev/chrome）
#   AGENT_CDP=http://127.0.0.1:9223 just chrome
#   just chrome --window-size=1440,900
#
# 然后（另开一个终端）：agent browser navigate https://example.com
chrome *ARGS:
    #!/usr/bin/env bash
    set -eu
    port="${AGENT_CDP_PORT:-9222}"
    bin=""
    for c in google-chrome-stable google-chrome chromium chromium-browser; do
      if command -v "$c" >/dev/null 2>&1; then bin=$(command -v "$c"); break; fi
    done
    if [ -z "$bin" ]; then
      echo "找不到 Chrome / Chromium（google-chrome-stable / chromium 都不在 PATH 里）" >&2
      exit 1
    fi
    profile="$PWD/.dev/chrome"
    mkdir -p "$profile"
    echo "无头 Chrome：$bin"
    echo "  profile=$profile  调试端口=$port"
    echo "  核心连的就是它（AGENT_CDP=http://127.0.0.1:$port）；Ctrl-C 退出"
    exec "$bin" --headless=new --remote-debugging-port="$port" \
      --user-data-dir="$profile" --no-first-run --no-default-browser-check \
      --disable-gpu --hide-scrollbars "$@"

# --- Web -------------------------------------------------------------------

web-install:
    cd {{web}} && bun install

web-dev:
    cd {{web}} && bun run dev

web-build:
    cd {{web}} && bun run build

web-check:
    cd {{web}} && bun run check

# --- 前端 DOM 审计 --------------------------------------------------------
#
# 用**真实渲染树**找「多余的 HTML 层级」。全本地：假 LLM（受控内容）+ 一个独立的核心
# 实例 + vite + headless Chrome —— 不碰手机、不碰 adb、不花 token。
#
# 判据两道：静态筛候选（只有一个子元素、无自有文字、盒子与子元素完全重合、无视觉副作用），
# 再对每个做**拔掉测试**（改成 display:contents 后比对整页每个元素的盒子）。
#
# 为什么不能只读源码：静态看着「盒子完全重合」的层，可能是父级 flex 的 item（被拉伸）、
# 或者子元素靠它的 flex-1 撑宽 —— 删了就变（实测 359 → 335.5px）。
#
#   just dom-audit                  # 全量（会话区 + 各个面板）
#   just dom-audit --no-panels      # 只看会话区
#   just dom-audit --snapshot /tmp/a.json   # 导几何快照
#   just dom-audit --diff /tmp/a.json       # 改完前端验证「布局一个像素都没动」
#
# 细节见 docs/07-debugging.md。
dom-audit *ARGS:
    #!/usr/bin/env bash
    set -eu
    scripts/dom-audit.sh "$@"

# --- 对着真机调试 ----------------------------------------------------------
#
# 核心是个只听 127.0.0.1 的 HTTP 服务，所以 adb forward 一下就能从电脑驱动它。
# 改 Rust 代码仍要重装（核心是 .so），但**调提示词、试沙箱、发设备指令、
# 看请求体**都不用。

# 无线调试：手机和电脑同一个 WiFi，手机上「开发者选项 → 无线调试」
#
#   just wifi-pair 192.168.0.34:41234 123456   # 首次（地址和码在手机屏幕上）
#   just wifi                                  # 之后每次
#   just wifi-tcpip                            # 或者：USB 插一次，之后自动走 WiFi
#
# 配过一次就不用再管了 —— 下面的 forward / phone / doctor 只依赖 adb，
# 不关心底下是 USB 还是无线。
wifi:
    scripts/phone-adb.sh connect

wifi-pair addr code:
    scripts/phone-adb.sh pair {{addr}} {{code}}

wifi-tcpip:
    scripts/phone-adb.sh tcpip

# 换完 Rust / 前端代码之后，一条命令回到「可调状态」：
# 重装 → 重启 App → 重新打开无障碍服务 → 转发端口
#
# 为什么要重新打开无障碍：force-stop 会解绑服务，重启后不重新挂上，
# 设备指令那条路就是死的。dev.sh 里会保留用户已有的其它无障碍服务。
restart:
    #!/usr/bin/env bash
    set -eu
    . scripts/dev.sh
    dev_restart && dev_a11y_on

# 把手机上核心的端口转发到本机（固定 18787；设备侧端口每次都变）
#
# 顺手做一件事：App 不在前台就把它叫到前台（**这是防冻的唯一办法**，见下）。
# 不想让它动手机：`AGENT_NO_WAKE=1 just forward`。
forward:
    scripts/phone-forward.sh

# 手机那头会不会被系统冻住 —— 远程回合挂死十有八九是它（不是 WebView）。
#
# App 一旦失去焦点，Android 的 cached app freezer 会在 60~75 秒后把它的**整个
# 进程**冻住：Rust 核心收不到请求、沙箱跑不了、WebView 也不动，而症状是
# 「不报错、不超时，就是挂住」，一碰手机瞬间全通。
#
#   just wake          # 看现在什么情况（焦点 / doze 白名单 / 冻没冻）
#   just wake on       # doze 白名单 + 叫到前台（开工前跑一次）
#   just wake off      # 摘掉 doze 白名单
#
# `just phone` / `just doctor` 走 phone-forward.sh，会自己跑 `wake auto`
# （已经在前台就什么都不做）。实测与两个机制的细节见 scripts/phone-wake.sh。
wake *ARGS:
    scripts/phone-wake.sh {{ARGS}}

# 手机上各个部件还活着吗（核心 / 模型 / 沙箱 / 设备 / 会话库）
#
#   just doctor          # 只做无副作用的检查
#   just doctor --deep   # 另外真跑一轮对话（花 token）
doctor *ARGS:
    #!/usr/bin/env bash
    set -eu
    base=$(scripts/phone-forward.sh)
    export AGENT_TOKEN=$(scripts/phone-token.sh)
    python3 scripts/agentctl.py --base "$base" --doctor "$@"

# 对着**手机上**跑着的核心说话
#
#   just phone "在沙箱里建个 fizzbuzz 跑一下"
#   just phone                     # 交互模式（/help 看命令）
phone *ARGS:
    #!/usr/bin/env bash
    set -eu
    base=$(scripts/phone-forward.sh)
    export AGENT_TOKEN=$(scripts/phone-token.sh)
    python3 scripts/agentctl.py --base "$base" "$@"

# 把手机上的界面开到桌面浏览器
#
# 前端资源是编进 .so 的，所以这就是手机上 WebView 看到的那一份；
# 好处是能用桌面 DevTools（元素审查、性能面板、网络）。
open:
    #!/usr/bin/env bash
    set -eu
    base=$(scripts/phone-forward.sh)
    # 先过 /auth 种上 cookie（核心的 /api 要凭据），它会跳回首页
    url="$base/auth?t=$(scripts/phone-token.sh)"
    echo "$base"
    (xdg-open "$url" >/dev/null 2>&1 || open "$url" >/dev/null 2>&1 || true) &

# --- 桌面沙箱 --------------------------------------------------------------

linux-setup:
    #!/usr/bin/env bash
    set -eu
    d=.dev/linux
    mkdir -p "$d"
    [ -f "$d/alpine-minirootfs.tar.gz" ] || curl -sSL -o "$d/alpine-minirootfs.tar.gz" \
      https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/x86_64/alpine-minirootfs-3.21.0-x86_64.tar.gz
    [ -d "$d/rootfs" ] || { mkdir -p "$d/rootfs"; tar xzf "$d/alpine-minirootfs.tar.gz" -C "$d/rootfs"; }
    echo "rootfs 就绪: $d/rootfs"
    command -v proot >/dev/null || echo "⚠️  没装 proot（Arch: sudo pacman -S proot-git）"

# --- Android ---------------------------------------------------------------

# 1) 生成 Kotlin 绑定（uniffi，proc-macro + library 模式，不需要 .udl）
#
#    先编一个**宿主**版本的 cdylib —— bindgen 要 dlopen 它读元数据。
android-bindings:
    #!/usr/bin/env bash
    set -eu
    # bindgen 会调 `cargo metadata`，必须在 crate 目录里跑
    cd {{core}}
    cargo build --lib
    # 宿主动态库的扩展名：Linux .so / macOS .dylib
    ext=so; [ "$(uname)" = Darwin ] && ext=dylib
    cargo run --features uniffi-bindgen --bin uniffi-bindgen -- \
      generate --library "target/debug/libagent_core.$ext" \
      --language kotlin --out-dir ../{{mod}}/src/main/java

# 2) 交叉编译 Rust 核心 → jniLibs
#
#    只出 arm64-v8a（真机）。要跑模拟器就把 -t x86_64 加回来。
#
#    编完务必验 16KB 对齐（R1）—— Play 对 targetSdk 35+ 有要求。
#    实测 cargo-ndk 4.x + NDK r27 已经默认对齐到 0x4000，但这个检查
#    很便宜，别省：
#      llvm-readelf -l {{jni}}/arm64-v8a/libagent_core.so | awk '/LOAD/{print $NF}'
android-libs:
    #!/usr/bin/env bash
    set -eu
    # cargo-ndk 同样要 cwd 在 crate 目录里
    cd {{core}}
    ts=(); IFS=',' read -ra arr <<< "{{abis}}"; for a in "${arr[@]}"; do ts+=(-t "$a"); done

    # 先清掉不在本次 ABI 清单里的目录。
    #
    # 为什么必须清：`abiFilters` 只在**打包那一步**过滤，
    # `mergeDebugNativeLibs` 照样把 jniLibs/ 里别的 ABI 合进来。
    # jniLibs/ 是生成物，`git status` 看不出来，`packaging.jniLibs.excludes`
    # 也排不掉（试过）。这里是唯一的源头。
    #
    # 注：这只能防「多带了一个 ABI」。APK 里那块 ~3.5MB 全零空洞还有第二个
    # 成因（增量打包），见 `android-debug` / `android-release` 里的 `rm -rf`。
    for d in ../{{jni}}/*/; do
      [ -d "$d" ] || continue
      a=$(basename "$d"); keep=0
      for k in "${arr[@]}"; do [ "$k" = "$a" ] && keep=1; done
      [ "$keep" = 1 ] || { echo "  清掉不在 ABI 清单里的 \"$a\"（见 docs/01 的 ABI 说明）"; rm -rf "$d"; }
    done

    cargo ndk "${ts[@]}" -o ../{{jni}} build --release --lib
    echo "--- 16KB 对齐检查（Play 对 targetSdk 35+ 有要求，R1）---"
    # NDK 的预编译工具目录：Linux 是 linux-x86_64，macOS（含 Apple 芯片）是 darwin-x86_64
    host=linux-x86_64; [ "$(uname)" = Darwin ] && host=darwin-x86_64
    "$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$host/bin/llvm-readelf" \
      -l "../{{jni}}/arm64-v8a/libagent_core.so" \
      | awk '/LOAD/{print "  align: " $NF}' | sort -u

# 3) 取 proot 二进制 + Alpine rootfs
#
#    ⚠️ proot 目前是**从本地 OpenMinis checkout 复制的**（NDK r27 编的，
#       和本机一致）。正式发布前必须自己编 —— 见 docs/02-linux-sandbox.md：
#       上游 proot 没有 Android 必需的 loader 处理，要用
#       OpenMinis/proot 或 termux/proot 的 fork。
android-assets:
    #!/usr/bin/env bash
    set -eu
    OM="${OPENMINIS_DIR:-$HOME/Code/lib/OpenMinis}/src/android/app/src/main"
    mkdir -p "{{assets}}" "{{jni}}/arm64-v8a"

    # Alpine rootfs：从官方源下载（aarch64）
    if [ ! -f "{{assets}}/alpine-minirootfs.tar.gz" ]; then
      curl -sSL -o "{{assets}}/alpine-minirootfs.tar.gz" \
        https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/aarch64/alpine-minirootfs-3.21.0-aarch64.tar.gz
    fi

    # proot + ELF loader：必须住在 nativeLibraryDir（Android 10+ 的 W^X
    # 只允许那里可执行）。伪装成 lib*.so 才能被 APK 安装器解压出来。
    if [ ! -f "{{jni}}/arm64-v8a/libproot.so" ]; then
      [ -d "$OM" ] || { echo "找不到 OpenMinis checkout（设 OPENMINIS_DIR）"; exit 1; }
      cp "$OM/jniLibs/arm64-v8a/libproot.so" \
         "$OM/jniLibs/arm64-v8a/libproot-loader.so" \
         "$OM/jniLibs/arm64-v8a/libproot-loader32.so" "{{jni}}/arm64-v8a/"
      echo "⚠️  proot 是从 $OM 复制的 —— 发布前需自建"
    fi

# 顺序不能反：`android-libs` 会把 `web/dist` **在编译期嵌进 .so**
# （rust-embed，见 core/src/assets.rs），所以必须先跑 web-build ——
# 反过来写的话每次装上去的都是**上一次**的前端（踩过：Kotlin 改动生效了，
# 但页面里 `window.__predictiveBack` 还是 undefined）。
#
# 下面是 assemble 前先删掉上一版的 APK / 打包中间产物 —— **不是**多此一举：
# AGP 的**增量打包**在原生 .so 变动时会留一块全零空洞。实测连着构建两次，
# debug 包从 9.0MB 涨到 12.6MB，多出来的 ~3.5MB 全是 0（入口在 `git status`
# 里完全看不出来）。每次从零打一遍的代价不到 1 秒。
android-debug: android-bindings web-build android-libs
    #!/usr/bin/env bash
    set -eu
    . scripts/jdk.sh
    rm -rf {{and}}/app/build/outputs/apk/debug {{and}}/app/build/intermediates/apk/debug
    cd {{and}} && ./gradlew :app:assembleDebug
    echo "APK: {{and}}/app/build/outputs/apk/debug/app-debug.apk"

android-install: android-debug
    #!/usr/bin/env bash
    set -eu
    . scripts/jdk.sh
    cd {{and}} && ./gradlew :app:installDebug

# release APK：R8 混淆/压缩 + 用 debug key 签（本地能直接装）。
# 原生核心两者都是 release（android-libs 里的 `--release`），区别只在
# Kotlin/dex 那层和签名。
#
# assemble 前同样删一遍（理由见 android-debug 上面那段）。
android-release: android-bindings web-build android-libs
    #!/usr/bin/env bash
    set -eu
    . scripts/jdk.sh
    rm -rf {{and}}/app/build/outputs/apk/release {{and}}/app/build/intermediates/apk/release
    cd {{and}} && ./gradlew :app:assembleRelease
    echo "APK: {{and}}/app/build/outputs/apk/release/app-release.apk"

android-install-release: android-release
    #!/usr/bin/env bash
    set -eu
    . scripts/jdk.sh
    cd {{and}} && ./gradlew :app:installRelease

android-log:
    adb logcat -s AgentApp AgentA11y AgentDeviceHost Rootfs AgentMain

# --- 模拟器（x86_64）------------------------------------------------------
#
# 真机是无线调试：手机一睡、用户一用、VPN 一抖，链路就掉，而且**不能在用户
# 正用手机时拿 input tap 去戳界面**。所以"改完就看"的 UI 验收放模拟器上。
#
# 限制：**沙箱跑不了**（proot + rootfs 都是 aarch64）。能验 UI / 浏览器 /
# 搜索 / LLM，不能验 bash / edit / read。见 scripts/emu.sh 顶部。
emu *ARGS:
    scripts/emu.sh {{ARGS}}

# 模拟器上装 **debug** 包，不是 release：
#   - WebView DevTools 开着（debug 才开）→ 可以用 CDP 精确驱动页面，
#     不用拿 `input tap` 盲戳坐标
#   - debug 可以用 `run-as` 读 App 私有文件（token / 会话库 / 截图）
emu-install:
    #!/usr/bin/env bash
    set -eu
    scripts/emu.sh start
    scripts/emu.sh wait
    export AGENT_ABIS=arm64-v8a,x86_64
    export ANDROID_SERIAL="$(scripts/emu.sh serial)"
    just android-debug
    . scripts/jdk.sh
    cd {{and}} && ./gradlew :app:installDebug
    "$ANDROID_HOME/platform-tools/adb" -s "$ANDROID_SERIAL" shell am start -n dev.vihv.agent/.MainActivity >/dev/null
    echo "装好了：export ANDROID_SERIAL=$ANDROID_SERIAL"

# --- 验证 ------------------------------------------------------------------

smoke:
    python3 -u scripts/smoke.py

all: test web-check web-build check android-debug
    @echo "✅ 全部通过"
