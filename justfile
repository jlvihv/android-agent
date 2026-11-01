# android-agent 构建编排
#
# 刻意**不让 Gradle 感知 Rust**：Gradle 只负责把 jniLibs/ 和 assets/ 打进 APK，
# cargo 由这里驱动。理由见 docs/01-tech-selection.md 的 D8。

set shell := ["bash", "-uc"]

# 没有这行，带 `*ARGS` 的 recipe 里 `"$@"` 是空的（踩过：`just phone --json "…"` 静默什么都没发）。
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
# 打进 APK 的 Alpine minirootfs 版本（只影响**新装**：已经装过的手机不会重解，
# 它们在 设置 → Linux 沙箱 里原地升级，见 core/src/alpine.rs）
alpine := "3.24.2"
alpine_branch := "v" + replace_regex(alpine, '\.[0-9]+$', '')

# NDK：从 SDK 里挑版本号最高的那个，避免依赖环境变量
# SDK：Linux 上习惯放 ~/Android/Sdk，macOS 上 Android Studio 默认放 ~/Library/Android/sdk
sdk_home := env_var_or_default("ANDROID_HOME", `for d in "$HOME/Android/Sdk" "$HOME/Library/Android/sdk"; do [ -d "$d" ] && { echo "$d"; exit; }; done`)
export ANDROID_HOME := sdk_home
ndk_home := env_var_or_default("AGENT_NDK_HOME", `ls -d "${ANDROID_HOME:-$HOME/Android/Sdk}"/ndk/*/ "$HOME"/Android/Sdk/ndk/*/ "$HOME"/Library/Android/sdk/ndk/*/ 2>/dev/null | sort -V | tail -1`)
export ANDROID_NDK_HOME := ndk_home
# cargo-ndk 会比对 NDK_HOME 和 ANDROID_NDK_HOME，不一致就骂（Arch 的 android-ndk
# 包会把 /opt/android-ndk 写进前者）。指到同一个地方，只影响 recipe 自己的环境。
export NDK_HOME := ndk_home

# JAVA_HOME 不在这里设。
#
# 用户环境里经常有个坏掉的 JAVA_HOME（比如指向没装的 Android Studio），
# 直接信任它会让 Gradle 起不来。所以下面每个用到 Gradle 的 recipe
# 自己探测一个能用的。

# --- Rust ------------------------------------------------------------------

check:
    cargo clippy --manifest-path {{core}}/Cargo.toml --all-targets -- -D warnings

# 清掉 7 天没用过的产物（dev / bindgen / NDK 各一份 target，实测 55GB）。
# 需要一次 `cargo install cargo-sweep`。
sweep:
    cargo sweep --time 7 --manifest-path {{core}}/Cargo.toml

# 后端 + 前端。前端那几个纯函数测的是窗口 / 下标对齐（连续两个 bug 都出在这），毫秒级。
test:
    cargo test --manifest-path {{core}}/Cargo.toml
    cd {{web}} && bun test

# 起核心：**桌面 = 这台电脑本身**（设备、bash、read/edit 都作用在本机；沙箱只属于
# Android，见 docs/02）。cwd 就是仓库根，模型的改动是**真的**，没有回收站。
#
# 默认对局域网开放（手机 App 能发现这台、输一次配对码就能用）；只想自己用：
#   AGENT_BIND=127.0.0.1 just dev
# 走 dev-fast（opt-level 1、不 LTO，改一行秒级）。要 release 二进制就用服务那套。
dev: (web "build")
    #!/usr/bin/env bash
    set -eu
    # 模型配置**不在这里给**：核心只认库（设置页 / `POST /api/llm`）。
    # `.env.local` 只剩中继密钥这类东西（AGENT_RELAY* / AGENT_TOKEN）。
    if [ -f .env.local ]; then set -a; . ./.env.local; set +a; fi
    cargo run --profile dev-fast --manifest-path {{core}}/Cargo.toml --bin agent-dev

# --- 中继（出门在外连家里的电脑，见 docs/12-relay.md） ----------------------

# 中继部署到哪台服务器。默认就是自己那台（ssh 用 vihv 登，sudo 免密）——
# 上次部署就是它，所以别再满世界找 root@ 了。换服务器：
#   AGENT_RELAY_HOST=user@1.2.3.4 just relay-deploy
relay_host := env_var_or_default("AGENT_RELAY_HOST", "vihv@129.211.71.14")

# 编出服务器上跑的中继：Linux x86_64 静态二进制（musl + rust-lld，macOS 上直接出）
relay-build:
    #!/usr/bin/env bash
    set -eu
    rustup target add x86_64-unknown-linux-musl >/dev/null
    cd relay
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
      cargo build --release --target x86_64-unknown-linux-musl
    echo "中继：relay/target/x86_64-unknown-linux-musl/release/agent-relay"

# 部署到服务器：装成 systemd 服务，打印家里要设的两行。不带参数就是上面那台
# （升级不会换密钥，`.env.local` 里那两行照旧，手机也不用重新配）
relay-deploy target=relay_host: relay-build
    scripts/deploy-relay.sh {{target}}

# 在本机跑一个中继（试用 / 调试）
# --- Web -------------------------------------------------------------------

# `just web build` / `dev` / `check` / `test` —— 都是 package.json 里的脚本。
web *ARGS:
    cd {{web}} && bun run {{ARGS}}

# 装依赖（`bun install` 不是 package.json 脚本，单独一个）。
web-install:
    cd {{web}} && bun install

# --- 前端 DOM 审计 --------------------------------------------------------
#
# 用**真实渲染树**找「多余的 HTML 层级」：假 LLM + 独立核心 + vite + headless Chrome，
# 全本地、不碰手机不花 token。判据是「拔掉这一层，整页每个元素的盒子都没动」。
# 细节见 docs/07-debugging.md。
dom-audit *ARGS:
    scripts/dom-audit.sh {{ARGS}}

# --- 对着真机调试 ----------------------------------------------------------
#
# 核心是个只听 127.0.0.1 的 HTTP 服务，所以 adb forward 一下就能从电脑驱动它。
# 改 Rust 代码仍要重装（核心是 .so），但**调提示词、试沙箱、发设备指令、
# 看请求体**都不用。

# 无线调试：手机和电脑同一个 WiFi，手机上「开发者选项 → 无线调试」
#
#   just wifi-pair 192.168.0.34:41234 123456   # 首次（地址和码在手机屏幕上）
#   just wifi 192.168.0.34:41234               # 连上（地址是「无线调试」页面上那行）
#   just wifi                                   # 之后每次（地址记着，掉线自连）
#   just wifi-tcpip                             # 或者：USB 插一次，之后自动走 WiFi
#
# 配过一次就不用再管了 —— 下面的 forward / phone / doctor 只依赖 adb，
# 不关心底下是 USB 还是无线。
wifi *ARGS:
    scripts/phone-adb.sh connect {{ARGS}}

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

# 常驻服务（开机就在）：install / uninstall / status / restart / logs
#
# macOS 走 launchd（~/Library/LaunchAgents/dev.vihv.android-agent.plist，日志在
# ~/Library/Logs/android-agent.log）；Linux 走 systemd 用户服务（android-agent.service）。
# 装成服务之后 `scripts/restart-core.sh` 只走系统那套重启，别手动 kill。
service *ARGS:
    scripts/service.sh {{ARGS}}

# 把手机上核心的端口转发到本机（固定 18787；设备侧端口每次都变）
#
# 顺手做一件事：App 不在前台就把它叫到前台（**这是防冻的唯一办法**，见下）。
# 不想让它动手机：`AGENT_NO_WAKE=1 just forward`。
forward:
    scripts/phone-forward.sh

# 手机那头会不会被系统冻住 —— 远程回合挂死十有八九是它：失去焦点 60~75 秒后
# 整个进程被冻住，症状是「不报错不超时，就是挂住」，一碰手机全通。
#   just wake / just wake on / just wake off。细节见 scripts/phone-wake.sh。
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

# --- Android ---------------------------------------------------------------

# 1) 生成 Kotlin 绑定（uniffi library 模式，不需要 .udl）：先编一个宿主版 cdylib 给它 dlopen。
android-bindings:
    #!/usr/bin/env bash
    set -eu
    # bindgen 会调 `cargo metadata`，必须在 crate 目录里跑
    cd {{core}}
    # 这一步带 `--features uniffi-bindgen`，feature 集一变 cargo 就把依赖树重编一遍
    # （实测 ~60s），和 `just dev` 的产物互相踩。给个单独的 target 目录，两边各自稳定。
    export CARGO_TARGET_DIR=target/bindgen
    cargo build --lib
    # 宿主动态库的扩展名：Linux .so / macOS .dylib
    ext=so; [ "$(uname)" = Darwin ] && ext=dylib
    # 没装 ktlint 时它会打一条「Unable to auto-format」的警告（不是错误），直接让它别试。
    fmt=(); command -v ktlint >/dev/null || fmt=(--no-format)
    cargo run --features uniffi-bindgen --bin uniffi-bindgen -- \
      generate --library "target/bindgen/debug/libagent_core.$ext" \
      --language kotlin --out-dir ../{{mod}}/src/main/java "${fmt[@]}"

# 2) 交叉编译 Rust 核心 → jniLibs。只出 arm64-v8a（模拟器要 x86_64：`just emu-install`）。
#
#    编完验 16KB 对齐（Play 对 targetSdk 35+ 有要求，R1）—— 这个检查很便宜，别省：
#      llvm-readelf -l {{jni}}/arm64-v8a/libagent_core.so | awk '/LOAD/{print $NF}'
android-libs:
    #!/usr/bin/env bash
    set -eu
    # cargo-ndk 同样要 cwd 在 crate 目录里
    cd {{core}}
    ts=(); IFS=',' read -ra arr <<< "{{abis}}"; for a in "${arr[@]}"; do ts+=(-t "$a"); done

    # 清掉不在本次 ABI 清单里的目录：`abiFilters` 只在打包那一步过滤，
    # `mergeDebugNativeLibs` 照样把别的 ABI 合进来（jniLibs 是生成物，git 看不出来）。
    for d in ../{{jni}}/*/; do
      [ -d "$d" ] || continue
      a=$(basename "$d"); keep=0
      for k in "${arr[@]}"; do [ "$k" = "$a" ] && keep=1; done
      [ "$keep" = 1 ] || { echo "  清掉不在 ABI 清单里的 \"$a\"（见 docs/01 的 ABI 说明）"; rm -rf "$d"; }
    done

    # release + fat LTO 是这里最慢的一步。release 本来就没有增量编译，让 sccache
    # 去命中上一次的产物是纯赚（cargo 在 RUSTC_WRAPPER 存在时会关掉增量，对 release
    # 无损）。没装 sccache 就照旧走。
    if command -v sccache >/dev/null 2>&1; then export RUSTC_WRAPPER=sccache; fi
    cargo ndk "${ts[@]}" -o ../{{jni}} build --release --lib
    echo "--- 16KB 对齐检查（Play 对 targetSdk 35+ 有要求，R1）---"
    # NDK 的预编译工具目录：Linux 是 linux-x86_64，macOS（含 Apple 芯片）是 darwin-x86_64
    host=linux-x86_64; [ "$(uname)" = Darwin ] && host=darwin-x86_64
    "$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$host/bin/llvm-readelf" \
      -l "../{{jni}}/arm64-v8a/libagent_core.so" \
      | awk '/LOAD/{print "  align: " $NF}' | sort -u

# 3) 取 proot 二进制 + Alpine rootfs。
#    ⚠️ proot 是从本地 OpenMinis checkout 复制的，发布前必须自己编（见 docs/02）。
android-assets:
    #!/usr/bin/env bash
    set -eu
    OM="${OPENMINIS_DIR:-$HOME/Code/lib/OpenMinis}/src/android/app/src/main"
    mkdir -p "{{assets}}" "{{jni}}/arm64-v8a"

    # Alpine rootfs 从官方源下载。版本戳放在 assets/ **外面**（里面的都会被打进 APK），
    # 否则改了上面的 `alpine` 也永远是旧包；顺手清掉别的名字的旧包（否则 APK 里两份）。
    stamp=.dev/alpine-asset-version
    if [ ! -f "{{assets}}/alpine-minirootfs.tar.gz" ] || [ "$(cat "$stamp" 2>/dev/null)" != "{{alpine}}" ]; then
      rm -f "{{assets}}"/alpine-minirootfs.tar*
      curl -fsSL --retry 5 --retry-all-errors -o "{{assets}}/alpine-minirootfs.tar.gz" \
        https://dl-cdn.alpinelinux.org/alpine/{{alpine_branch}}/releases/aarch64/alpine-minirootfs-{{alpine}}-aarch64.tar.gz
      mkdir -p .dev && echo "{{alpine}}" > "$stamp"
      echo "Alpine minirootfs {{alpine}} 已下载"
    fi

    # proot + ELF loader 必须住在 nativeLibraryDir（Android 10+ 的 W^X），伪装成 lib*.so。
    if [ ! -f "{{jni}}/arm64-v8a/libproot.so" ]; then
      [ -d "$OM" ] || { echo "找不到 OpenMinis checkout（设 OPENMINIS_DIR）"; exit 1; }
      cp "$OM/jniLibs/arm64-v8a/libproot.so" \
         "$OM/jniLibs/arm64-v8a/libproot-loader.so" \
         "$OM/jniLibs/arm64-v8a/libproot-loader32.so" "{{jni}}/arm64-v8a/"
      echo "⚠️  proot 是从 $OM 复制的 —— 发布前需自建"
    fi

# 顺序不能反：`android-libs` 在编译期把 `web/dist` 嵌进 .so（rust-embed），
# 反过来装上去的永远是上一次的前端。
# assemble 前删掉上一版 APK / 中间产物：AGP 增量打包在 .so 变动时会留一块全零
# 空洞（实测 9.0MB → 12.6MB），从零打一遍不到 1 秒。
android-debug: android-bindings (web "build") android-libs
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

# release APK：R8 混淆/压缩 + 签名（见 app/build.gradle.kts 里的 signingConfigs）。
# 原生核心也是 release（thin LTO，见 core/Cargo.toml），区别只在
# Kotlin/dex 那层和签名。
#
# assemble 前同样删一遍（理由见 android-debug 上面那段）。
android-release: android-bindings (web "build") android-libs
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
# 「改完就看」的 UI 验收放模拟器（真机是无线调试，会掉线，也不能在用户用手机时去戳）。
# 限制：**沙箱跑不了**（proot/rootfs 都是 aarch64），验不了 bash / edit / read。
emu *ARGS:
    scripts/emu.sh {{ARGS}}

# 装 debug 包：WebView DevTools 开着（能用 CDP 精确驱动），`run-as` 也能读私有文件。
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
