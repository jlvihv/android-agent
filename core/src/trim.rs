//! 把分配器**攥着但已经不用的**内存还给系统。
//!
//! ## 为什么需要
//!
//! 一个会话的消息是几 MB 级的一堆**小块**：JSON 解出来的 `String` / `Vec`、
//! 每一段 tool 输出、每一条 assistant 内容。`evict_idle` 把它们一起放掉时，
//! 这些块各自 `free` 掉了，但它们在 arena 里是**散开的** —— glibc 只在
//! 收回的那块正好挨着堆顶时才收缩 brk（`M_TRIM_THRESHOLD` = 128 KB），
//! 中间的空洞它一概不动。于是 RSS 停在峰值上，看上去像泄漏，其实全是碎片。
//!
//! 实测（本机，77 个会话、连开 6 个大会话再等着它们被放掉）：业务真正在用的
//! 只有 2–5 MB，RSS 却曾经停在 51 MB；接上这一步之后同一个流程自动回落到
//! 24 MB（拿那会儿 `inuse` 是 2.4 MB）。换分配器不管用：jemalloc 更差
//! （97 MB）、tcmalloc 也差（60 MB），`MALLOC_ARENA_MAX=2` 只省 9 MB
//! 而且得靠每个部署记得配。
//!
//! ## 为什么在这个时机
//!
//! 只在「刚放掉了一些会话」之后调（`lib.rs` 的落盘定时器里，`evict_idle`
//! 返回非零时）。那一刻刚好是「一大坨内存在语义上不再需要了」，是最值得还
//! 的一次；定时 trim 则是每 2 秒去碰一次堆，白付代价还抓不准时机。

/// 把 `free` 掉、但还留在分配器 arena 里的内存还给系统。
///
/// 返回**是不是真的还了一些**（没有可还的块时返回 `false`，不是错误）。
///
/// 还的是「堆顶那段连续的」和「能整块 `madvise` 出去的」；arena 里被活数据
/// **夹在中间**的空洞照样留着 —— 所以是「大部分」，不是「全部」。
///
/// 只在 glibc 上有这个入口：musl / macOS 的分配器自己会 `munmap`，安卓的
/// bionic 也没有 —— 那些目标上编出来就是永远返回 `false` 的空函数（那边确实
/// 没有「攥着不放的空间」要还），调用点不用写 `cfg`。
pub fn release_free_memory() -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: `malloc_trim` 只动分配器自己**空闲**的块（收缩 brk、
        // 整块 `madvise` 出去），不碰任何活着的分配；参数是「堆顶要留
        // 多少空闲」，0 = 不留。
        // 它会拿 glibc 的 arena 锁 —— 别在 tokio worker 上直接调。
        unsafe { libc::malloc_trim(0) == 1 }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 「能安全调用、还完照样能分配」—— 真的还了多少 kB 是分配器的事，
    /// 测不了（换分配器、换时机都可能没有可还的块）。
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
}
