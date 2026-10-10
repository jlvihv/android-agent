// 版本刻意保守：AGP 8.13 而不是 9.x —— 9.x 有一批 DSL 破坏性变更，
// 我们这个工程不需要那些新特性，稳定优先。
plugins {
    id("com.android.application") version "8.13.2" apply false
    id("org.jetbrains.kotlin.android") version "2.4.21" apply false
}
