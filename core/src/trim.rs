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
//! （97 MB）、tcmalloc 也差（60 MB）、`MALLOC_ARENA_MAX=2` 只省 9 MB
//! 而且得靠每个部署记得配。
//!
//! ## 各平台分别怎么做
//!
//! 三家分配器**都有**「把空闲的还回去」的入口，只是名字不同；没有的那个
//! 恰恰是 musl（它每个大块单独 `mmap`，`free` 即 `munmap`，本来就没这个坑）：
//!
//! - glibc（桌面 Linux）：`malloc_trim(0)`
//! - bionic/scudo（安卓）：`mallopt(M_PURGE, 0)`（API 28 起；`minSdk = 28`
//!   就是为这个，见 `android/app/build.gradle.kts`）
//! - musl / macOS / 别的：编成空函数（它们自己会还，调了也没坏处）
//!
//! ## 为什么在这个时机
//!
//! 两个「一大坨内存在语义上不再需要了」的时刻，都不去定时碰堆：
//!
//! - 刚放掉了一些会话（`lib.rs` 的落盘定时器里，`evict_idle` 返回非零时）
//! - **刚跑完一个回合**（`agent::TurnGuard::drop`）—— 一轮对话里最大的临时块
//!   （请求体的 base64、工具输出的整段捕获、图片解码的中间图）都在那前后放掉。
//!   之前没人在这儿还，arena 就一直顶着峰值挂到下一次 evict：实测跑了 2 天 14
//!   小时的进程 arena 要了 107 MB，而 `in use` 只有 24 MB。
//!
//! 定时 trim（比如每 2 秒碰一次堆）不做：白付代价还抓不准时机。

/// 把 `free` 掉、但还留在分配器 arena 里的内存还给系统。
///
/// 返回**是不是真的还了一些**（没有可还的块时返回 `false`，不是错误）。
///
/// 还的是「堆顶那段连续的」和「能整块 `madvise` 出去的」；arena 里被活数据
/// **夹在中间**的空洞照样留着 —— 所以是「大部分」，不是「全部」。
/// 安卓上 scudo 的回收粒度是内存块大小分级，同样不保证「一块不剩」。
pub fn release_free_memory() -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: `malloc_trim` 只动分配器自己**空闲**的块（收缩 brk、
        // 整块 `madvise` 出去），不碰任何活着的分配；参数是「堆顶要留
        // 多少空闲」，0 = 不留。
        // 它会拿 glibc 的 arena 锁 —— 别在 tokio worker 上直接调。
        unsafe { libc::malloc_trim(0) == 1 }
    }
    #[cfg(target_os = "android")]
    {
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
        // SAFETY: 和 glibc 那边同理 —— 只动分配器自己的空闲块，不碰活着的
        // 分配；第二个参数对 M_PURGE 是被忽略的。
        // 它会拿 scudo 的全局锁 —— 和 `malloc_trim` 一样别在 worker 上直接调。
        unsafe { mallopt(-101, 0) == 1 }
    }
    #[cfg(not(any(all(target_os = "linux", target_env = "gnu"), target_os = "android")))]
    {
        false
    }
}

/// 一次内存账：内核看到的 vs 分配器眼里**真正在用**的。
///
/// 为什么要有这个：RSS 里有一大半是分配器攥着、业务早就不用的碎片（本机实测
/// 107 MB 的 arena 里 82 MB 是空闲的），只看 RSS 会把它当成泄漏、去优化一个
/// 根本不存在的问题。两个数摆在一起才分得清「业务真占了多少」和「分配器还不
/// 肯还」。字段是 `None` = 这个平台问不到（安卓问不到分配器那份、macOS 问不
/// 到 RSS）。
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

#[cfg(target_os = "android")]
fn allocator_usage() -> (Option<u64>, Option<u64>) {
    // scudo（bionic）没有 `mallinfo2`。它有个老名字的 `mallinfo`，字段是
    // `c_int`（溢出的风险由我们踩不起：动辄上百 MB 也能装下 2GB，先不管），
    // 这里不用它 —— 与其给一个会被截断的数，不如老实说「问不到」。
    //
    // 关键：**这里返回 None 不能让 `worth_trimming` 变成 false**。安卓上
    // `M_PURGE` 正是为碎片准备的，问不到账就该「直接还」而不是「不还」——
    // 见 `worth_trimming` 的写法。
    (None, None)
}

#[cfg(not(any(all(target_os = "linux", target_env = "gnu"), target_os = "android")))]
fn allocator_usage() -> (Option<u64>, Option<u64>) {
    (None, None)
}

/// `worth_trimming` 的**规则本体**，账从外面递进来（`None` = 问不到）。
///
/// 拆成「量」和「判」两半：`report()` 在桌面上总是给 `Some`，安卓那条「问不到
/// 账」的路在桌面上跑不到 —— 不拆的话那半边永远没人验。
/// **问不到账的时候（`None`）一律当「还」**，别当「不还」：安卓（scudo）就是问不到的
/// 那种，而那边的 `mallopt(M_PURGE)` 正是为碎片准备的。之前这里写的是
/// `is_some_and(..)` —— 于是安卓上 `None` 直接短路成 false，`mallopt` 一次都没跑过。
fn worth_trimming_given(free: Option<u64>) -> bool {
    const MIN_FREE: u64 = 2 << 20;
    free.is_none_or(|f| f >= MIN_FREE)
}

/// 在当前运行时里把「还内存」丢给阻塞池，**不等**。
///
/// 调用点是同步的（回合结束的 `Drop`），而 `malloc_trim` 要拿 arena 锁、
/// 不能坐在 tokio worker 上 —— 所以走 `spawn_blocking`。不在运行时里
/// （单元测试）就同步还，那种时候没人在等。
pub fn release_soon() {
    // **只量一次**：`report()` 要读一个 `/proc` 文件、还要调 `mallinfo2`，
    // 两者都会拿锁。量完把这份账递给后面的判断和日志，别各读一遍。
    let free = report().free;
    if !worth_trimming_given(free) {
        return;
    }
    match tokio::runtime::Handle::try_current() {
        Ok(h) => drop(h.spawn_blocking(move || trim_and_log(free))),
        Err(_) => drop(trim_and_log(free)),
    }
}

/// 真还了一次就记一笔（debug 级：平时不看，查内存问题时能看出「什么时候还过」）。
///
/// `free` 是还**之前**量到的空闲量（调用方量的，别在这里再量一次）。
fn trim_and_log(free: Option<u64>) -> bool {
    let ok = release_free_memory();
    if ok {
        // 安卓问不到账（`free` 是 `None`），不强行编个「0 MB」出来。
        match free {
            Some(b) => tracing::debug!("还了分配器的空闲内存（之前有 {} MB 空闲）", b / (1 << 20)),
            None => tracing::debug!("还了分配器的空闲内存"),
        }
    }
    ok
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

    /// `release_soon` 在没有运行时的时候也得能调（同步兜底那条路）。
    #[test]
    fn releasing_soon_outside_runtime_is_harmless() {
        let big = vec![7u8; 8 << 20];
        drop(big);
        release_soon();
        assert_eq!(vec![1u8; 16].len(), 16);
    }

    /// 账要能读出来：Linux/glibc 上 RSS 和分配器的数都问得到。
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn report_reads_both_sides() {
        let r = report();
        let rss = r.rss.expect("Linux 上要能读到 RSS");
        let in_use = r.in_use.expect("glibc 上要能读到 uordblks");
        assert!(rss > 0);
        // 分配器发的总不会比进程 RSS 还多（共享页、二进制映像都算在 RSS 里）
        assert!(
            in_use < rss + (64 << 20),
            "in_use={in_use} 远大于 rss={rss}，账反了"
        );
    }

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

    /// **问不到账的时候要「还」，不是「不还」。**
    ///
    /// 这是安卓那条路的关键：scudo 问不到 `fordblks`（`free` 是 `None`），而
    /// 安卓的 `mallopt(M_PURGE)` 正是为碎片准备的。这里曾经写成
    /// `free.is_some_and(|f| f >= MIN_FREE)` —— `None` 短路成 false，于是手机上
    /// 一次都没还过（桌面反而正常，所以很难发现）。
    #[test]
    fn unknown_ledger_still_trims() {
        // 问不到 → 还（安卓、以及将来任何没 mallinfo 的分配器）
        assert!(worth_trimming_given(None), "问不到账就该直接还");
        // 问得到且值得 → 还
        assert!(worth_trimming_given(Some(8 << 20)));
        // 问得到但没多少可还 → 不折腾
        assert!(!worth_trimming_given(Some(512 << 10)));
    }

    /// `release_soon` 在**每个平台**都得编得过、调得动、不 panic。
    ///
    /// 它只差一个 cfg 分支就会变成「恒定 false」（上一个测试盖规则，这个盖
    /// 接线），两个一起才拦得住那类「桌面绿、手机静默失效」。
    #[test]
    fn release_soon_is_callable() {
        release_soon();
    }
}
