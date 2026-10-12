plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

/** 要出哪些 ABI（`AGENT_ABIS=arm64-v8a,x86_64 just emu-install` 那种）。 */
val agentAbis =
    (System.getenv("AGENT_ABIS") ?: "arm64-v8a")
        .split(",").map { it.trim() }.filter { it.isNotEmpty() }

/** AGP 认得的全部 ABI，用来算出「不在 agentAbis 里的」。 */
val knownAbis =
    listOf("arm64-v8a", "armeabi-v7a", "armeabi", "x86", "x86_64", "mips", "mips64")

android {
    namespace = "dev.vihv.agent"
    compileSdk = 36

    defaultConfig {
        applicationId = "dev.vihv.agent"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"

        // 只出 arm64-v8a。
        //
        // 之前带 x86_64 是为了模拟器，但代价是真机上白带一份 9.3MB 的核心
        // （压缩后 ~4MB APK）。要跑模拟器时临时把 x86_64 加回来即可。
        // 出哪些 ABI 由 AGENT_ABIS 决定（见 justfile）：默认只出 arm64，
        // 模拟器要 x86_64 时 `AGENT_ABIS=arm64-v8a,x86_64 just emu-install`
        ndk {
            abiFilters += agentAbis
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            // 本地装 release 用 debug key 签（AGP 默认就有一把
            // ~/.android/debug.keystore）。和 debug 包同一把钥匙，能互相覆盖安装。
            // 正式发布前要换成自己的 keystore。
            signingConfig = signingConfigs.getByName("debug")
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlin {
        compilerOptions {
            jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17)
        }
    }

    packaging {
        jniLibs {
            // 不在 ABI 清单里的 .so 明确排掉。
            //
            // 主要针对 JNA 那个 AAR 自带的 armeabi / mips / x86 等（它全 ABI
            // 都带）。**注意**：这条排不掉 `jniLibs/` 里别的 ABI 造成的那块
            // APK 零填充 —— 那个坑在 `mergeDebugNativeLibs` 那一层，
            // 真正的修法在 justfile 的 `android-libs` 里清目录。
            excludes += knownAbis.filterNot { it in agentAbis }.map { "lib/$it/**" }

            // ⚠️ 关键：必须是 true。
            //
            // 默认（false）时 .so 留在 APK 里由 linker 直接 mmap，磁盘上
            // 没有实体文件 —— 而我们要 **exec** nativeLibraryDir 里的
            // libproot.so。只有解压到磁盘才有可执行路径。
            //
            // Android 10+ 的 W^X 只允许 nativeLibraryDir 可执行，
            // 所以这是唯一的路（见 docs/02-linux-sandbox.md）。
            useLegacyPackaging = true
        }
    }

    // cargo-ndk 的输出目录（由 justfile 的 android-libs 生成）
    sourceSets["main"].jniLibs.srcDir("src/main/jniLibs")

    lint {
        // 我们刻意不用 AndroidX；没必要求最新版本
        abortOnError = false
    }
}

dependencies {
    // 唯一一个依赖。
    //
    // uniffi 0.32 的 Kotlin 后端生成的是 JNA 绑定（不是 JNI），
    // 所以必须带 JNA。这是选 UniFFI 的代价 —— 换来的是不用手写 JNI。
    // AAR 里带全 ABI 的 libjnidispatch.so，abiFilters 会裁掉多余的。
    implementation("net.java.dev.jna:jna:5.17.0@aar")
}
