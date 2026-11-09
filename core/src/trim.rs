//! 把分配器**攥着但已经不用的**内存还给内核。**只有安卓用得上。**
//!
//! ## 现在只有安卓在用它（2026-11-09 收窄）
//!
//! 桌面那份二进制（`agent-dev`）换成了 **musl 静态**（为什么见 README 的
//! 「桌面核心编成 musl 静态」那段）：musl 的 malloc 对超过阈值的块直接 `mmap`、
//! `free` 即 `munmap`，没有 arena 这层，**本来就没什么可还的**。所以桌面上这个模块
//! 整个不编进去（`lib.rs` 里是 `cfg(target_os = "android")`），`release_soon()` 的
//! 两个调用点也跟着一起变成空操作 —— 不是「调了没用」，是**根本不存在**。
//!
//! 之前桌面走 glibc 时这里是有意义的：`malloc_trim(0)` 能把停在峰值上的 RSS 打下来
//! （实测 119 MB → 4.9 MB）。但那次测量同时说明它不是长效手段 —— glibc 的 4 个
//! tokio worker + 4 个 blocking 线程各有一块 arena，`malloc_trim` 只收拾调用者那块；
//! 再加上这台机器 `transparent_hugepage/enabled=[always]`，散落的小对象会被按 2 MB
//! 记账（实测：256 MB 区域每 2 MB 只写 1 字节，RSS 就是 264 MB，活数据 128 KB）。
//! 那 78 MB 里 70 MB 是这么来的，活数据只有 2.7 MB。换 musl 是从根上把这两条都去掉。
//!
//! ## 安卓为什么必须留着
//!
//! bionic 用 scudo，**没有** musl 那种「大块走 `mmap`、`free` 即 `munmap`」的性质，
//! 也不像 glibc 有 `malloc_trim`。安卓上「把空闲内存还给内核」只有一个入口：
//! `mallopt(M_PURGE, 0)`（API 28 起，`minSdk = 28` 就是为它提的，见
//! `android/app/build.gradle.kts` 和 `docs/02-linux-sandbox.md`）。**别因为桌面上
//! 用不着了就把这个文件删掉** —— 手机上会立刻回退到「arena 顶着峰值不还」。
//!
//! ## 为什么需要（安卓上的场景）
//!
//! 一个会话的消息是几 MB 级的一堆**小块**：JSON 解出来的 `String` / `Vec`、
//! 每一段 tool 输出、每一条 assistant 内容。`evict_idle` 把它们一起放掉时，
//! 这些块各自 `free` 掉了，但 scudo 的分级缓存照样攥着不放。于是 RSS 停在峰值上，
//! 看上去像泄漏，其实全是碎片。
//!
//! 实测（glibc 时代的本机，同样适用于 scudo 的定性结论）：77 个会话、连开 6 个大会话
//! 再等着它们被放掉，业务真正在用的只有 2–5 MB，RSS 却停在 51 MB；接上 trim 之后
//! 回落到 24 MB（那会儿 `inuse` 是 2.4 MB）。
//!
//! ## 为什么在这个时机
//!
//! 两个「一大坨内存在语义上不再需要了」的时刻，都不去定时碰堆：
//!
//! - 刚放掉了一些会话（`lib.rs` 的落盘定时器里，`evict_idle` 返回非零时）
//! - **刚跑完一个回合**（`agent::TurnGuard::drop`）—— 一轮对话里最大的临时块
//!   （请求体的 base64、工具输出的整段捕获、图片解码的中间图）都在那前后放掉。
//!
//! 定时 trim（比如每 2 秒碰一次堆）不做：白付代价还抓不准时机。

#![cfg(target_os = "android")]

/// 把 scudo 攥着的空闲内存还给内核。
///
/// 返回**是不是真的还了一些**（没有可还的块时返回 `false`，不是错误）。
///
/// 安卓上 scudo 的回收粒度是内存块大小分级，不保证「一块不剩」。
///
/// 故意**不 pub**：外部一律走 [`release_soon`]。那一个自带「搬到阻塞池」——
/// 直接把这一条暴露出去，等于把「`mallopt` 要拿 scudo 的全局锁、不能坐在
/// worker 上」这个约束外派给每个调用方去记得。
fn release_free_memory() -> bool {
    // bionic 的 mallopt 不是 `libc` crate 暴露的那几个（它只导出了
    // `mallinfo` / `malloc_info`），自己声明一下。
    unsafe extern "C" {
        fn mallopt(param: libc::c_int, value: libc::c_int) -> libc::c_int;
    }
    // `M_PURGE` = -101（定义在 bionic 的 `<malloc.h>`，API 28 起）：
    // 「立即把所有不在用的内存还给内核」。比它更狠的 `M_PURGE_ALL`
    // 要 API 34，而且官方文档说可能慢一倍以上（它会把所有东西都翻
    // 一遍），这里用不上。
    //
    // SAFETY: 只动分配器自己的空闲块，不碰活着的分配；第二个参数对 M_PURGE
    // 是被忽略的。它会拿 scudo 的全局锁 —— 别在 worker 上直接调。
    unsafe { mallopt(-101, 0) == 1 }
}

/// 在当前运行时里把「还内存」丢给阻塞池，**不等**。
///
/// 调用点是同步的（回合结束的 `Drop`），而 `mallopt` 要拿 scudo 的全局锁、
/// 不能坐在 tokio worker 上 —— 所以走 `spawn_blocking`。不在运行时里
/// （单元测试）就同步还，那种时候没人在等。
pub fn release_soon() {
    match tokio::runtime::Handle::try_current() {
        Ok(h) => drop(h.spawn_blocking(trim_and_log)),
        Err(_) => drop(trim_and_log()),
    }
}

/// 真还了一次就记一笔（debug 级：平时不看，查内存问题时能看出「什么时候还过」）。
fn trim_and_log() {
    if release_free_memory() {
        tracing::debug!("还了分配器攥着的空闲内存");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 「能安全调用、还完照样能分配」—— 真的还了多少是分配器的事，
    /// 测不了（换机型、换时机都可能没有可还的块）。
    #[test]
    fn releasing_is_harmless() {
        let big = vec![7u8; 8 << 20];
        assert_eq!(big[0], 7);
        drop(big);
        release_free_memory();
        // 还完之后堆还是好的
        let again = vec![9u8; 1 << 20];
        assert_eq!(again[0], 9);
        assert_eq!(again.len(), 1 << 20);
    }

    /// `release_soon` 在没有运行时的时候也得能调（同步兜底那条路）。
    #[test]
    fn releasing_soon_outside_runtime_is_harmless() {
        let big = vec![7u8; 8 << 20];
        drop(big);
        release_soon();
        assert_eq!(vec![1u8; 16].len(), 16);
    }

    /// `release_soon` 在**每个平台**都得编得过、调得动、不 panic。
    ///
    /// 这个模块只在安卓上编（文件顶那条 `cfg`），所以 `cargo test` 在桌面上
    /// 根本不会跑到这里 —— 它真正的验证场是 `cargo ndk` 那一遍和真机。
    /// 留着是为了在安卓上跑测试时盯着「接线没断」。
    #[test]
    fn release_soon_is_callable() {
        release_soon();
    }
}
