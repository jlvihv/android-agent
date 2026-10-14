//! 桌面开发用二进制。
//!
//! ```sh
//! just web-build   # 先构建前端（rust-embed 在编译期嵌入）
//! just dev         # cargo run --bin agent-dev
//! ```
//!
//! 打开 <http://127.0.0.1:8787> 就是完整 App —— 和手机上 WebView 看到的
//! 是同一份东西。这是 D3（同源 localhost）最大的好处。

use anyhow::Context;

/// 注意：**不用 `#[tokio::main]`**。那个宏按 CPU 核数开 worker（这台机器 32 核
/// → 33 个线程），而核心里全是等 IO 的活。runtime 由 `agent_core::runtime()`
/// 统一构造（和安卓同一条路径、同一个线程预算）。
fn main() -> anyhow::Result<()> {
    agent_core::init_tracing();
    agent_core::runtime()?.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let port: u16 = std::env::var("AGENT_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8787);

    // 这台机器（这个用户）上只跑一个核心。两个宿主核心不会有任何报错，但会静默地
    // 互相干扰（mDNS 同名服务互相挤掉、两边写库时内存态互相覆盖），所以抢不到
    // 就直接不起，并把现在跑着的那个说清楚。见 `agent_core::single`。
    let _single = match agent_core::single::acquire(&agent_core::single::default_path(), port) {
        Ok(g) => g,
        Err(who) => {
            eprintln!();
            eprintln!("  已经有核心在跑了：{}", who.describe());
            eprintln!("  这台机器上只允许一个 —— 两个会静默地互相干扰：mDNS 广播互相挤掉、");
            eprintln!("  会话库的内存态互相覆盖（两边都不报错，只是行为怪）。先停掉它再起：");
            eprintln!(
                "    {}",
                who.kill_hint()
                    .unwrap_or_else(|| "kill <那个核心的 pid>".into())
            );
            eprintln!(
                "  确实要同时跑两个（DOM 审计 / 基准测试）：{}=1",
                agent_core::single::MULTI_ENV
            );
            eprintln!();
            std::process::exit(2);
        }
    };

    let core = agent_core::Core::desktop();

    // 听在哪：**宿主模式默认对局域网开放**（0.0.0.0）—— 手机 App 的「选择机器」
    // 能自动找到这台、输一次配对码就能用它，不用另起一个特殊命令。只想自己用：
    // `AGENT_BIND=127.0.0.1`。沙箱模式（`just dev`，在电脑上复现手机）默认只听本机。
    let host_mode = core.state.linux().is_some_and(|s| s.is_host());
    let bind: std::net::IpAddr = match std::env::var("AGENT_BIND") {
        Ok(v) if !v.trim().is_empty() => v
            .trim()
            .parse()
            .with_context(|| format!("AGENT_BIND={v} 不是 IP 地址"))?,
        _ if host_mode => std::net::Ipv4Addr::UNSPECIFIED.into(),
        _ => std::net::Ipv4Addr::LOCALHOST.into(),
    };
    let lan = !bind.is_loopback();
    // 凭据：只在本机时默认不设防（只有自己在用）；设了 AGENT_TOKEN 就和手机上
    // 一样要凭据。**听在局域网上就必须有** —— 不然同一个 Wi-Fi 里谁都能在这台
    // 电脑上跑命令（宿主模式下就是你的真机器）。没给的话用库里那把（宿主模式
    // 有库，重启不变，手机上收藏的地址一直能用），没库就现编一把。
    let env_token = std::env::var("AGENT_TOKEN").ok().filter(|t| !t.is_empty());
    // 凭据是给局域网的；电脑自己（loopback）照旧免登录。自己给了 AGENT_TOKEN 的
    // 意思是「本机也要」，那就不放行
    if lan && env_token.is_none() {
        core.state
            .trust_loopback
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let token = match (env_token, lan) {
        (Some(t), _) => Some(t),
        (None, true) => Some(
            core.state
                .auth()
                .token
                .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()),
        ),
        (None, false) => None,
    };
    core.state.set_token(token.clone());
    let actual = core
        .serve_on(bind, port)
        .await
        .context("启动 HTTP 服务失败")?;

    println!();
    println!("  android-agent core v{}", agent_core::VERSION);
    let trusted = core
        .state
        .trust_loopback
        .load(std::sync::atomic::Ordering::Relaxed);
    match &token {
        Some(t) if !trusted => println!("  → http://127.0.0.1:{actual}/auth?t={t}"),
        _ => println!("  → http://127.0.0.1:{actual}"),
    }
    // 局域网：广播自己 + 配对码（手机 App 的「选择机器」自动发现、第一次连时输码）。
    // `_advert` 活到 main 结束：Drop 就停广播
    let mut _advert = None;
    if lan {
        let pairing = std::sync::Arc::new(agent_core::desktop::Pairing::new());
        if let Ok(mut g) = core.state.pairing.write() {
            *g = Some(pairing.clone());
        }
        let name = agent_core::desktop::advert_name();
        let mut advert = agent_core::desktop::advertise(&name, actual, &core.state.instance_id());
        // 广播进程起来后**立刻退出**是常事（avahi-daemon 没跑、这个名字已经被这台
        // 机器上另一个核心占了），而它只往 stderr 说一句就不再出声 —— 不确认一下
        // 的话，下面那句「手机上点「{name}」」就是空头支票（这台机器上就这么空转了
        // 很久）。隔一下问一句，不在就把原因原样打出来。见 `desktop::Advert`。
        let quiet = match advert.as_mut() {
            Some(a) => {
                tokio::time::sleep(agent_core::desktop::ADVERT_SETTLE).await;
                a.failure()
            }
            None => {
                Some("系统里没有广播工具（Linux: avahi-publish-service，macOS: dns-sd）".into())
            }
        };
        _advert = advert;
        match quiet {
            None => println!(
                "  → 手机 App：侧栏底栏的机器标签 → 选择机器，点「{name}」，配对码 {}",
                pairing.code()
            ),
            Some(why) => {
                println!("  ⚠️  局域网广播没起来：{why}");
                println!(
                    "     手机 App 只能连广播找得到的电脑（没有手动输入地址），所以这次连不上；"
                );
                println!("     手机浏览器打开下面的地址还能用。");
                println!(
                    "     起好系统的 mDNS 服务再重启核心（Linux: systemctl enable --now avahi-daemon）；"
                );
                println!("     这台机器上已经有另一个核心在广播的话，给新的换个名字：AGENT_ADVERT_NAME=…");
            }
        }
        let ips: Vec<std::net::IpAddr> = if bind.is_unspecified() {
            lan_ips().into_iter().map(Into::into).collect()
        } else {
            vec![bind]
        };
        let t = token.as_deref().unwrap_or_default();
        if ips.is_empty() {
            println!("  → 手机上打开：http://<这台电脑的局域网 IP>:{actual}/auth?t={t}");
        }
        for ip in ips {
            println!("  → 手机上打开：http://{ip}:{actual}/auth?t={t}");
        }
        println!("    （明文 HTTP：只在自己信得过的 Wi-Fi 里用；地址里的 t= 就是钥匙。");
        println!("      只想自己用：AGENT_BIND=127.0.0.1）");
    }
    let device = core.state.device().map(|d| d.name()).unwrap_or_default();
    println!("  （设备：{device}；Ctrl-C 退出）");
    println!();

    // Ctrl-C **和 SIGTERM** 都要走正常退出路径：`Drop` 才会把自己起的那个
    // 无头 Chrome 收掉（`kill <pid>` 默认发 SIGTERM，不处理的话浏览器会变成
    // 孤儿进程）。真被 SIGKILL 就没这个机会了 —— 那种情况靠下次启动时的
    // `cdp::sweep_orphan_chromes` 收。
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
    Ok(())
}

/// 这台电脑在局域网里的地址：各个网卡上**私有网段**的 IPv4。
///
/// 不能用「往外连时系统挑哪张网卡」（UDP connect 那个办法）：开着全局代理
/// （Clash 的 TUN 模式之类）的机器上挑中的是虚拟网卡，地址是 198.18.x.x，
/// 手机根本连不到 —— 在这台 Mac 上就是这么打出一个错地址的。所以老实枚举
/// 网卡，只认 192.168/16、10/8、172.16/12。
fn lan_ips() -> Vec<std::net::Ipv4Addr> {
    let mut out = Vec::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs 填链表，用完 freeifaddrs；只读 ifa_addr 指向的 sockaddr_in
    unsafe {
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut p = ifap;
        while !p.is_null() {
            let ifa = &*p;
            let up = ifa.ifa_flags & (libc::IFF_UP as u32) != 0;
            if up && !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if ip.is_private() && !out.contains(&ip) {
                    out.push(ip);
                }
            }
            p = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out
}
