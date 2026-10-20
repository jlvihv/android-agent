# JNA —— uniffi 生成的绑定靠它做 Native.register，名字不能被混淆
-keep class com.sun.jna.** { *; }
-keepclassmembers class * extends com.sun.jna.** { public *; }
-keep class * implements com.sun.jna.** { *; }
-dontwarn java.awt.**

# uniffi 生成的绑定
-keep class uniffi.** { *; }
-keep class dev.vihv.agent.** { *; }

# 保留回调接口的实现（Rust 会反调它们）
-keepclassmembers class * implements uniffi.agent_core.DeviceHost { public *; }

# 保留回调接口的实现（Rust 会反调它们）—— P1 之后多了三条
-keepclassmembers class * implements uniffi.agent_core.BrowserHost { public *; }
-keepclassmembers class * implements uniffi.agent_core.PhoneHost { public *; }
-keepclassmembers class * implements uniffi.agent_core.ShizukuHost { public *; }

# Shizuku —— **反射调隐藏 API，名字一个字都不能变**
#
# `ShizukuManager.newProcess()` 调的是
# `Shizuku::class.java.getDeclaredMethod("newProcess", …)`：按**字符串**找方法。
# R8 把方法改名成 `a()` 之后，`getDeclaredMethod("newProcess")` 就抛
# NoSuchMethodException —— 表现是「adb 身份拿不到」，而 debug 包好的。
# 这正是 docs/15 §4 说的那个坑的混淆版本。
-keep class rikka.shizuku.** { *; }
-keep class rikka.sui.** { *; }
-keepclassmembers class rikka.shizuku.** { *; }
-keepclassmembers class rikka.sui.** { *; }

# Shizuku 的 binder 通道靠 AIDL 生成的 Stub/Proxy 按类名找
-keep class moe.shizuku.** { *; }
-dontwarn rikka.shizuku.**
-dontwarn rikka.sui.**
