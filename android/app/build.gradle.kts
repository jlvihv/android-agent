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

    signingConfigs {
        // 签名用的 key：默认 `~/Downloads/debug.keystore`（本机一直用的那把），
        // 要换就 `AGENT_KEYSTORE=<路径>`。口令/别名沿用 debug key 那套：
        // storepass=android、alias=androiddebugkey（PKCS12 里两者是同一个口令）。
        val home = System.getProperty("user.home")
        val key = (System.getenv("AGENT_KEYSTORE")?.takeIf { it.isNotBlank() }
            ?: "$home/Downloads/debug.keystore")
            .replaceFirst(Regex("^~"), home)
        if (file(key).isFile) {
            create("local") {
                storeFile = file(key)
                storePassword = System.getenv("AGENT_KEYSTORE_PASS") ?: "android"
                keyAlias = System.getenv("AGENT_KEY_ALIAS") ?: "androiddebugkey"
                keyPassword = System.getenv("AGENT_KEY_PASS") ?: "android"
            }
        }
    }

    buildTypes {
        // debug 也认 AGENT_KEYSTORE：手机上的包是别的机器签的时候，`installDebug` 照样能覆盖装
        debug {
            signingConfig = signingConfigs.findByName("local") ?: signingConfigs.getByName("debug")
        }
        release {
            isMinifyEnabled = true
            // 本地装 release 和 debug 用同一把 key（AGP 自带的 debug key 作为
            // 上面的 keystore 不在时的退路，两者能互相覆盖安装）。
            // 正式发布前要换成自己的 keystore。
            signingConfig = signingConfigs.findByName("local") ?: signingConfigs.getByName("debug")
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

    // Shizuku / Sui / AXManager —— 拿 adb（uid 2000）甚至 root 身份跑系统命令，
    // 走的是 `agent shizuku exec` 那条（见 core/src/shizuku.rs）。
    //
    // `api` 是客户端（`Shizuku.pingBinder()` / `newProcess`），`provider` 是
    // 那个用于接 binder 的 ContentProvider（manifest 里也要声明一个）。
    // 只有手机上有意义：桌面宿主模式没有这一组命令。
    implementation("dev.rikka.shizuku:api:13.1.5")
    implementation("dev.rikka.shizuku:provider:13.1.5")

    // 唯一一个测试依赖（`./gradlew :app:testDebugUnitTest`）。
    // 只给纯函数用：Shizuku 的状态机（`ShizukuManager.decideState`）——
    // 那三条分支要靠真机把 Shizuku 装/卸/杀一遍才能验，写成纯函数就是为了这里能测。
    testImplementation("junit:junit:4.13.2")
}
