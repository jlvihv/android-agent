//! 这个进程占了多少内存 —— `/api/debug/state` 的 `memory` 字段就来自这里。
//!
//! ## 为什么不留在 `trim` 里
//!
//! `trim` 是「把空闲内存还给内核」的**动作**，只在安卓上有意义（见那个文件的说明）。
//! 而「我现在占了多少」这件事**每个平台都问得出来**，跟分配器无关，也不该因为
//! 桌面换成了 musl 就跟着消失 —— 调试内存问题时第一眼要看的就是它。
//!
//! ## 两个数为什么要一起给
//!
//! `rss` 是内核看到的，`inUse` 是分配器眼里**业务真正在用**的。两者差的那一大块
//! 是分配器攥着、业务早就不用的碎片（**不是泄漏**）。只看 RSS 会去优化一个不存在
//! 的问题 —— 之前就是这么误判过：78 MB 的 RSS 里活数据只有 2.7 MB，剩下的是
//! glibc arena + THP 大页放大的假象（见 README 的「桌面核心编成 musl 静态」）。
//!
//! 字段是 `None` = 这个平台问不到：
//!
//! - `in_use` / `free`：musl（桌面现在就是）和 scudo（安卓）都没有 `mallinfo2`，
//!   老实说问不到，不硬编一个 0 出来
//! - `rss`：非 Linux 内核（macOS）没有 `/proc`

/// 一次内存账：内核看到的 vs 分配器眼里**真正在用**的。
#[derive(Debug, Clone, Copy)]
pub struct Report {
    /// 进程 RSS（字节）。
    pub rss: Option<u64>,
    /// 分配器发放出去、还没 `free` 的字节（glibc 的 `uordblks + hblkhd`）。
    pub in_use: Option<u64>,
    /// 分配器攥在 arena 里、已经没人用的字节（glibc 的 `fordblks`）。
    pub free: Option<u64>,
}

/// 量一次。便宜（读一个 `/proc` 文件 + 一次 `mallinfo2`），随时可调。
pub fn report() -> Report {
    let (in_use, free) = allocator_usage();
    Report {
        rss: rss_bytes(),
        in_use,
        free,
    }
}

// 安卓也是 Linux 内核，`/proc/self/status` 一样有（读不到就当 None）。
// **不能写 `cfg(target_os = "linux")`**：安卓的 `target_os` 是 `"android"`，
// 那样写手机上就永远问不到 RSS。
#[cfg(any(target_os = "linux", target_os = "android"))]
fn rss_bytes() -> Option<u64> {
    // `/proc/self/status` 比 `statm` 好读：带单位（kB），不用自己乘页大小。
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = s.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn rss_bytes() -> Option<u64> {
    None
}

/// 只有 glibc 报得出分配器的账。
///
/// 桌面那份二进制现在是 musl 编的（见 README），所以**桌面上这条
/// 走不到** —— 它现在是给「哪天又回 glibc」留的。musl 的 `report()` 给
/// `in_use: None, free: None`，这是对的：它没有 arena 这层，问不到也没有意义。
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn allocator_usage() -> (Option<u64>, Option<u64>) {
    // SAFETY: `mallinfo2` 只读 glibc 自己的账本，不碰任何分配。
    let m = unsafe { libc::mallinfo2() };
    // uordblks = arena 里在用的；hblkhd = 走 mmap 的大块（也是在用）。
    (
        Some(m.uordblks as u64 + m.hblkhd as u64),
        Some(m.fordblks as u64),
    )
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn allocator_usage() -> (Option<u64>, Option<u64>) {
    // musl：没有 `mallinfo2`，而且**不需要** —— 它超过阈值的块直接 `mmap`，
    // `free` 即 `munmap`，没有「攥着不还」这回事。
    // scudo（安卓）：有个老名字的 `mallinfo`，但字段是 `c_int`，动辄上百 MB
    // 会被截断 —— 与其给一个错的数，不如老实说问不到。
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Linux 内核上（含安卓）RSS 都要读得到。
    ///
    /// 盯着一个真踩过的坑：这里原来写的是 `cfg(target_os = "linux")`，而安卓的
    /// `target_os` 是 `"android"` —— 手机上 RSS 永远是 `None`。这种错编译不报、
    /// 测试在桌面上也不会红，只在手机上静默失效。
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn rss_is_readable_on_linux_kernels() {
        assert!(
            report().rss.is_some(),
            "Linux 内核上 /proc/self/status 一定有 VmRSS"
        );
    }

    /// 分配器的账：glibc 上问得到，别的上面老实给 `None`。
    ///
    /// 这条同时盯着「musl 上别硬编一个 0」—— `None` 和 `Some(0)` 的区别是
    /// 「问不到」和「真的一点没用」，调试时看错就白查半天。
    #[test]
    fn allocator_ledger_matches_the_platform() {
        let (in_use, free) = allocator_usage();
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        {
            assert!(in_use.is_some() && free.is_some(), "glibc 上两样都问得到");
        }
        #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
        {
            assert!(in_use.is_none() && free.is_none(), "问不到就给 None，别编数");
        }
    }

    /// glibc 上账不能反：分配器发的不会比进程 RSS 还多那么多
    /// （共享页、二进制映像都算在 RSS 里）。
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn report_is_consistent_on_glibc() {
        let r = report();
        let rss = r.rss.expect("Linux 上要能读到 RSS");
        let in_use = r.in_use.expect("glibc 上要能读到 uordblks");
        assert!(rss > 0);
        assert!(
            in_use < rss + (64 << 20),
            "in_use={in_use} 远大于 rss={rss}，账反了"
        );
    }
}
