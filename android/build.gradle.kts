// AGP 9.5.1。
//
// 从 8.13.2 升上来的理由不是追新，是 SDK 的 `package.xml` 早就写成 schema /04 了
// （platforms;android-36、build-tools 36、platform-tools、ndk 27 都是），而 AGP 8.x
// 内置的 sdklib 31.13.2 只认识 /03 —— 每次构建都会打一条
// 「This version only understands SDK XML versions up to 3」。AGP 9.0 起带
// sdklib 32.x，认识 /04，警告消失。
//
// AGP 9 的破坏性变更里只踩到两条，都在 app/build.gradle.kts 里注明了：
//   1. Kotlin 支持内置，`org.jetbrains.kotlin.android` 插件必须删掉（留着直接报错）。
//   2. 不再读环境变量 ANDROID_NDK_HOME，必须显式写 `ndkVersion`。
//
// 但“内置”不等于“版本随 AGP 走”：AGP 9.5.1 内置的 KGP / kotlin-stdlib 是 **2.2.10**，
// 而本工程升级前用的是 2.4.21 —— 直接删插件会把 Kotlin 降回去（静默的）。
// 所以下面显式把 KGP 顶到 2.4.21，编译器和 stdlib 都跟着它走。
// 升/降 Kotlin 只改这一行。
buildscript {
    repositories {
        google()
        mavenCentral()
    }
    dependencies {
        classpath("org.jetbrains.kotlin:kotlin-gradle-plugin:2.4.21")
    }
}

plugins {
    id("com.android.application") version "9.5.1" apply false
}
