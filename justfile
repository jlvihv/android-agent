# android-agent 构建编排
#
# 刻意**不让 Gradle 感知 Rust**：Gradle 只负责把 jniLibs/ 和 assets/ 打进 APK，
# cargo 由这里驱动。理由见 docs/01-tech-selection.md 的 D8。

set shell := ["bash", "-uc"]

core  := "core"
web   := "web"
and   := "android"
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

# --- Web -------------------------------------------------------------------

web-install:
    cd {{web}} && bun install

web-dev:
    cd {{web}} && bun run dev

web-build:
    cd {{web}} && bun run build

web-check:
    cd {{web}} && bun run check

# --- 对着真机调试 ----------------------------------------------------------
#
# 核心是个只听 127.0.0.1 的 HTTP 服务，所以 adb forward 一下就能从电脑驱动它。
# 改 Rust 代码仍要重装（核心是 .so），但**调提示词、试沙箱、发设备指令、
# 看请求体**都不用。

# 把手机上核心的端口转发到本机（端口是启动时随机分的，从 logcat 里捞）
#
#   just forward          # 自动找
#   just forward 41234    # 手动指定
forward port="":
    #!/usr/bin/env bash
    set -eu
    p=$(scripts/phone-port.sh "{{port}}")
    adb forward "tcp:$p" "tcp:$p" >/dev/null
    echo "core → http://127.0.0.1:$p"

# 对着**手机上**跑着的核心说话
#
#   just phone "在沙箱里建个 fizzbuzz 跑一下"
#   just phone                     # 交互模式（/help 看命令）
phone *ARGS:
    #!/usr/bin/env bash
    set -eu
    p=$(scripts/phone-port.sh)
    adb forward "tcp:$p" "tcp:$p" >/dev/null
    python3 scripts/agentctl.py --base "http://127.0.0.1:$p" "$@"

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
#    只出 arm64-v8a + x86_64（决策 D9）。
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
    cargo ndk -t arm64-v8a -t x86_64 -o ../{{jni}} build --release --lib
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

android-debug: android-bindings android-libs web-build
    #!/usr/bin/env bash
    set -eu
    . scripts/jdk.sh
    cd {{and}} && ./gradlew :app:assembleDebug
    echo "APK: {{and}}/app/build/outputs/apk/debug/app-debug.apk"

android-install: android-debug
    #!/usr/bin/env bash
    set -eu
    . scripts/jdk.sh
    cd {{and}} && ./gradlew :app:installDebug

android-log:
    adb logcat -s AgentApp AgentA11y AgentDeviceHost Rootfs AgentMain

# --- 验证 ------------------------------------------------------------------

smoke:
    python3 -u scripts/smoke.py

all: test web-check web-build check android-debug
    @echo "✅ 全部通过"
