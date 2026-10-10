plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.vihv.agent"
    compileSdk = 36

    defaultConfig {
        applicationId = "dev.vihv.agent"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"

        // 只出这两个。armeabi-v7a 和 x86 跳过（决策 D9）。
        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
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
