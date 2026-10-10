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
