//! `dumpsys` / `pm` / `settings` 那些**文本输出**的解析。
//!
//! 为什么单独一个模块，而不是塞进 `shizuku.rs`：这里是全套东西里**最容易坏**的
//! 部分 —— 系统版本一升、机型一换，格式就可能对不上。所以它必须能拿一段真文本
//! 单独测（`core/tests/fixtures/shizuku/*.txt` 是从 Pixel 10 / Android 17 上
//! 抓下来的真样本）。
//!
//! ## 一条硬规矩：解析不出来就交回原文
//!
//! 每个 `parse_*` 返回 `Option`。拿不到关键字段时**不做部分解析、不编默认值**，
//! 直接 `None` —— 调用方（`shizuku::run_group`）会退回原始的 `exec` 输出。
//!
//! 理由：解析器坏了的时候，"看起来成功但字段是空的"比"直接给原文"糟得多。
//! 模型看到 `电量: -1` 会当成真的没电；看到一段 `dumpsys battery` 原文，
//! 它自己就能读出来，而且**立刻知道解析器没跟上这台机器**。

use serde_json::{json, Map, Value};

/// 从一个 `dumpsys` 的 key-value 块里取值。
///
/// 行形状是 `  key: value`（前面两个空格，大小写不统一：`level` 和 `Capacity level`
/// 混着）。按 key 找时**忽略大小写和空格**，因为同一个概念在不同版本里写法不同
/// （`Capacity level` / `capacity_level`）。
fn kv_lookup(text: &str, key: &str) -> Option<String> {
    let want = norm_key(key);
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if norm_key(k) == want {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// `dumpsys package` 里是 `key=value`（不是冒号），单独一套。
///
/// 不能和 [`kv_lookup`] 合并：`dumpsys battery` 用冒号、`dumpsys package` 用等号，
/// 而 `=` 也出现在别处（`flags=[ ... ]`），按冒号硬解会全读错。
fn eq_lookup(text: &str, key: &str) -> Option<String> {
    let want = norm_key(key);
    for line in text.lines() {
        for (k, v) in split_eq_pairs(line) {
            if norm_key(&k) == want {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// 把一行里的 `k=v` 对都拆出来（一行可能挤好几个：`versionCode=1 minSdk=26`）。
fn split_eq_pairs(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // 跳过空白
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let ks = i;
        while i < chars.len() && chars[i] != '=' && !chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() || chars[i] != '=' {
            // 这个 token 没有等号，跳到下一个空白
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            continue;
        }
        let key: String = chars[ks..i].iter().collect();
        i += 1; // 跳过 '='
                // 值到下一个空白为止；但 `[ ... ]` / `{ ... }` 里的空格不算分隔
        let vs = i;
        if i < chars.len() && matches!(chars[i], '[' | '{') {
            let close = if chars[i] == '[' { ']' } else { '}' };
            let mut depth = 0;
            while i < chars.len() {
                if chars[i] == chars[vs] {
                    depth += 1;
                } else if chars[i] == close {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                i += 1;
            }
        } else {
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
        }
        let val: String = chars[vs..i].iter().collect();
        if !key.is_empty() {
            out.push((key, val));
        }
    }
    out
}

fn norm_key(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '_')
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 一个整数字段（`level: 53`）。
fn kv_int(text: &str, key: &str) -> Option<i64> {
    kv_lookup(text, key)?.parse().ok()
}

// ---------------------------------------------------------------------------
// device battery
// ---------------------------------------------------------------------------

/// 电池状态 → 中文短字段。
///
/// 一个字段都不给时返回 `None`（那说明这根本不是 `dumpsys battery` 的输出）。
///
/// 数字翻成人话（`status: 2` → `充电中`）是**这里唯一值得做的事**：
/// 状态码表是 Android 的枚举，模型每次都要现查才能确定 5 是"满电"还是"未充电"。
pub fn parse_battery(text: &str) -> Option<Value> {
    let level = kv_int(text, "level")?;
    let mut m = Map::new();
    m.insert("电量".into(), json!(format!("{level}%")));
    // temperature 是**十分之一摄氏度**（377 = 37.7℃）—— 这个单位坑不说就会误读
    if let Some(t) = kv_int(text, "temperature") {
        m.insert("温度".into(), json!(format!("{:.1}℃", t as f64 / 10.0)));
    }
    if let Some(s) = kv_int(text, "status") {
        let s = match s {
            1 => "未知",
            2 => "充电中",
            3 => "放电中",
            4 => "未充电",
            5 => "已充满",
            _ => "未知",
        };
        m.insert("状态".into(), json!(s));
    }
    if let Some(h) = kv_int(text, "health") {
        let h = match h {
            2 => "良好",
            3 => "过热",
            4 => "已损坏",
            5 => "过压",
            6 => "未指定故障",
            7 => "过冷",
            _ => "未知",
        };
        m.insert("健康".into(), json!(h));
    }
    if let Some(v) = kv_int(text, "voltage") {
        m.insert("电压".into(), json!(format!("{:.3}V", v as f64 / 1000.0)));
    }
    // 谁在供电：取**第一个** true 的，全 false 就是没在充电
    let source = [
        "AC powered",
        "USB powered",
        "Wireless powered",
        "Dock powered",
    ]
    .iter()
    .find(|k| kv_lookup(text, k).as_deref() == Some("true"));
    if let Some(k) = source {
        let name = match *k {
            "AC powered" => "充电器",
            "USB powered" => "USB",
            "Wireless powered" => "无线",
            _ => "底座",
        };
        m.insert("供电".into(), json!(name));
    }
    Some(Value::Object(m))
}

// ---------------------------------------------------------------------------
// device info / usage
// ---------------------------------------------------------------------------

/// `getprop` 里挑出人要看的那几个属性 → 短字段。
///
/// 为什么要有：`getprop | grep ro.product` 会带出 `_for_attestation` /
/// `_family` 之类的噪音（实测 14 行里 8 行是噪音），而模型要的是
/// 「什么牌子、什么型号、什么系统版本」三件事。
pub fn parse_device_info(text: &str) -> Option<Value> {
    // `[ro.product.model]: [Pixel 10]`
    let get = |key: &str| -> Option<String> {
        for line in text.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix(&format!("[{key}]: ")) else {
                continue;
            };
            let v = rest.trim().trim_start_matches('[').trim_end_matches(']');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
        None
    };
    let mut m = Map::new();
    for (label, key) in [
        ("品牌", "ro.product.brand"),
        ("型号", "ro.product.model"),
        ("设备", "ro.product.device"),
        ("系统", "ro.build.version.release"),
        ("SDK", "ro.build.version.sdk"),
    ] {
        if let Some(v) = get(key) {
            m.insert(label.into(), json!(v));
        }
    }
    if m.is_empty() {
        return None;
    }
    Some(Value::Object(m))
}

/// `dumpsys activity activities` → 顶层 Activity 的包名/类名。
pub fn parse_top_activity(text: &str) -> Option<Value> {
    // 格式：topResumedActivity=ActivityRecord{251986963 u0 com.pkg/.Cls t21558}
    // 老版本可能只有 mResumedActivity
    for key in ["topResumedActivity", "mResumedActivity"] {
        for line in text.lines() {
            let Some(pos) = line.find(key) else { continue };
            let s = &line[pos..];
            let Some(at) = s.find(" u0 ") else { continue };
            let rest = s[at + 4..].trim();
            // `com.pkg/.Cls t21558` → 取到空格为止
            let comp = rest.split_whitespace().next()?;
            let (pkg, cls) = comp.split_once('/')?;
            let cls = if let Some(c) = cls.strip_prefix('.') {
                format!("{pkg}.{c}") // `.MainActivity` 是简写，补全成全名
            } else {
                cls.to_string()
            };
            return Some(json!({ "包名": pkg, "页面": cls }));
        }
    }
    None
}

/// `pm list users` → `[{id, 名字, 运行中}]`。
pub fn parse_users(text: &str) -> Option<Value> {
    // `UserInfo{0:机主:4c13} running`（名字里可能带冒号，flags 是十六进制）
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(start) = line.find("UserInfo{") else {
            continue;
        };
        let Some(end) = line[start..].find('}') else {
            continue;
        };
        let inner = &line[start + "UserInfo{".len()..start + end];
        // id 是第一个冒号前，flags 是最后一个冒号后，中间都算名字
        let Some((id, rest)) = inner.split_once(':') else {
            continue;
        };
        let Ok(id) = id.parse::<i64>() else { continue };
        let (name, flags) = match rest.rsplit_once(':') {
            Some((n, f)) => (n, f),
            None => (rest, ""),
        };
        out.push(json!({
            "id": id,
            "名字": name,
            "运行中": line[end..].contains("running"),
            "flags": flags,
        }));
    }
    if out.is_empty() {
        return None;
    }
    Some(Value::Array(out))
}

/// `dumpsys display` → 每块屏的尺寸/密度/刷新率。
///
/// 只认 `DisplayDeviceInfo{…}` 那一行：`mBaseDisplayInfo` / `mOverrideDisplayInfo`
/// 是同一块屏的两种视图（后者被 `wm size` 改过），混进来会出现三行重复数据。
pub fn parse_display(text: &str) -> Option<Value> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(start) = line.find("DisplayDeviceInfo{") else {
            continue;
        };
        let block = &line[start..];
        let mut m = Map::new();
        // 名字：第一个引号里的内容
        if let Some(q) = block.find('"') {
            if let Some(e) = block[q + 1..].find('"') {
                m.insert("屏".into(), json!(&block[q + 1..q + 1 + e]));
            }
        }
        // `1080 x 2424`
        if let Some(dim) = find_dim(block) {
            m.insert("分辨率".into(), json!(format!("{}x{}", dim.0, dim.1)));
        }
        if let Some(d) = find_after(block, "density ", |s| {
            s.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|x| x.parse::<i64>().ok())
        }) {
            m.insert("密度".into(), json!(d));
        }
        // `renderFrameRate 120.00001`
        if let Some(f) = find_after(block, "renderFrameRate ", |s| {
            s.split(|c: char| !c.is_ascii_digit() && c != '.')
                .next()
                .and_then(|x| x.parse::<f64>().ok())
        }) {
            m.insert("刷新率".into(), json!(format!("{f:.0}Hz")));
        }
        if let Some(r) = find_after(block, "rotation ", |s| {
            s.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|x| x.parse::<i64>().ok())
        }) {
            m.insert("旋转".into(), json!(r));
        }
        if m.contains_key("分辨率") {
            out.push(Value::Object(m));
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(Value::Array(out))
}

/// 找 `<n> x <m>` 这种尺寸。
fn find_dim(s: &str) -> Option<(i64, i64)> {
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        // 跳到一个数字开头
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let w: String = bytes[start..i].iter().collect();
        // 期望 " x "
        let mut j = i;
        while j < bytes.len() && bytes[j] == ' ' {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == 'x' {
            j += 1;
            while j < bytes.len() && bytes[j] == ' ' {
                j += 1;
            }
            let hs = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > hs {
                let h: String = bytes[hs..j].iter().collect();
                if let (Ok(w), Ok(h)) = (w.parse::<i64>(), h.parse::<i64>()) {
                    // 太小的是别的数字（比如 flags），尺寸至少三位
                    if w >= 100 && h >= 100 {
                        return Some((w, h));
                    }
                }
            }
        }
        i = start + 1;
    }
    None
}

/// 在 `marker` 后面取一个数。
fn find_after<T>(s: &str, marker: &str, f: impl Fn(&str) -> Option<T>) -> Option<T> {
    let at = s.find(marker)? + marker.len();
    f(&s[at..])
}

// ---------------------------------------------------------------------------
// package
// ---------------------------------------------------------------------------

/// `pm list packages` → 包名数组（去掉 `package:` 前缀，排序）。
pub fn parse_packages(text: &str) -> Option<Value> {
    let mut v: Vec<String> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("package:").map(|s| s.to_string()))
        .filter(|s| !s.is_empty())
        .collect();
    if v.is_empty() {
        return None;
    }
    v.sort();
    Some(json!(v))
}

/// `dumpsys package <pkg>` → 几个关键字段。
pub fn parse_package_info(text: &str) -> Option<Value> {
    let mut m = Map::new();
    // `versionCode=1 minSdk=26 targetSdk=35` 挤在一行
    if let Some(v) = kv_int_inline(text, "versionCode") {
        m.insert("versionCode".into(), json!(v));
    }
    if let Some(v) = eq_lookup(text, "versionName") {
        m.insert("versionName".into(), json!(v));
    }
    if let Some(v) = kv_int_inline(text, "targetSdk") {
        m.insert("targetSdk".into(), json!(v));
    }
    if let Some(v) = kv_int_inline(text, "minSdk") {
        m.insert("minSdk".into(), json!(v));
    }
    for (label, key) in [
        ("首装", "firstInstallTime"),
        ("更新", "lastUpdateTime"),
        ("数据目录", "dataDir"),
        ("安装者", "installerPackageName"),
    ] {
        if let Some(v) = eq_lookup(text, key) {
            // `installerPackageName=null` 是"侧载/系统装"，别当成字符串 "null"
            let v = if v == "null" {
                "（无）".to_string()
            } else {
                v
            };
            m.insert(label.into(), json!(v));
        }
    }
    if m.is_empty() {
        return None;
    }
    Some(Value::Object(m))
}

/// `key=value` 挤在一行里的那种（`versionCode=1 minSdk=26 targetSdk=35`）的**数字**值。
fn kv_int_inline(text: &str, key: &str) -> Option<i64> {
    for line in text.lines() {
        for (k, v) in split_eq_pairs(line) {
            if norm_key(&k) == norm_key(key) {
                let num: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
                return num.parse().ok();
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// permission
// ---------------------------------------------------------------------------

/// `dumpsys package <pkg>` → 权限清单。
///
/// 只认 `runtime permissions:` 那一段里带 `granted=` 的行 —— `requested permissions:`
/// 是"申请了哪些"，不代表给了；两者混在一起会让模型以为权限都开着。
pub fn parse_permissions(text: &str) -> Option<Value> {
    let mut out = Vec::new();
    let mut in_runtime = false;
    for line in text.lines() {
        let t = line.trim();
        if t.ends_with("permissions:") {
            in_runtime = t.starts_with("runtime");
            continue;
        }
        if !in_runtime {
            continue;
        }
        if t.is_empty() {
            in_runtime = false;
            continue;
        }
        // `android.permission.CAMERA: granted=true, flags=[ ... ]`
        let Some((name, rest)) = t.split_once(':') else {
            continue;
        };
        if !rest.contains("granted=") {
            continue;
        }
        let granted = rest.contains("granted=true");
        out.push(json!({ "权限": name.trim(), "已授予": granted }));
    }
    if out.is_empty() {
        return None;
    }
    // 给了的排前面：模型通常只关心"有哪些权限能用了"
    out.sort_by_key(|v| v["已授予"].as_bool() != Some(true));
    Some(Value::Array(out))
}

// ---------------------------------------------------------------------------
// notification
// ---------------------------------------------------------------------------

/// `dumpsys notification --noredact` → 每条通知的 包名/id/tag/importance。
pub fn parse_notifications(text: &str) -> Option<Value> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(start) = line.find("NotificationRecord(") else {
            continue;
        };
        let block = &line[start..];
        let pkg = field_of(block, "pkg=")?;
        let mut m = Map::new();
        m.insert("包名".into(), json!(pkg));
        if let Some(id) = field_of(block, "id=") {
            m.insert("id".into(), json!(id));
        }
        if let Some(tag) = field_of(block, "tag=") {
            if tag != "null" {
                m.insert("tag".into(), json!(tag));
            }
        }
        if let Some(u) = field_of(block, "user=UserHandle{") {
            m.insert("用户".into(), json!(u));
        }
        if let Some(i) = field_of(block, "importance=") {
            m.insert("重要度".into(), json!(i));
        }
        out.push(Value::Object(m));
    }
    if out.is_empty() {
        return None;
    }
    Some(Value::Array(out))
}

/// `marker` 后面那个值（到空格或分隔符为止）。
fn field_of(s: &str, marker: &str) -> Option<String> {
    let at = s.find(marker)? + marker.len();
    let v: String = s[at..]
        .chars()
        .take_while(|c| !matches!(c, ' ' | ',' | '}' | ')'))
        .collect();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

/// `settings list <ns>` → 一堆 `key=value` 行 → 对象。
///
/// 值一律按**字符串**保留：`settings` 里的东西类型不定（`0`/`1`、字符串、空），
/// 硬转数字会让 `null` 变成 0。
pub fn parse_settings_list(text: &str) -> Option<Value> {
    let mut m = Map::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            if !k.is_empty() {
                m.insert(k.to_string(), json!(v.trim()));
            }
        }
    }
    if m.is_empty() {
        return None;
    }
    Some(Value::Object(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 从真机上抓下来的样本（Pixel 10 / Android 17），见 `core/tests/fixtures/shizuku`。
    fn fixture(name: &str) -> String {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/shizuku")
            .join(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读不到 {}: {e}", p.display()))
    }

    #[test]
    fn battery_gives_short_human_fields() {
        let v = parse_battery(&fixture("battery.txt")).expect("该解析出来");
        // 只留几个字段，别把 dumpsys 那一屏照抄过来
        assert!(v.as_object().unwrap().len() <= 6, "{v}");
        assert_eq!(v["电量"], json!("53%"));
        // 377 → 37.7℃（十分之一度，这个单位不说就会误读成 377 度）
        assert_eq!(v["温度"], json!("37.7℃"));
        assert_eq!(v["状态"], json!("充电中")); // status: 2
        assert_eq!(v["健康"], json!("良好")); // health: 2
        assert_eq!(v["电压"], json!("4.171V")); // 4171 mV
        assert_eq!(v["供电"], json!("充电器")); // AC powered: true
    }

    #[test]
    fn battery_returns_none_on_foreign_text() {
        assert!(parse_battery("这不是 dumpsys\n随便一段").is_none());
        // 有 key 但没 level：不猜，交回原文
        assert!(parse_battery("  temperature: 300\n").is_none());
    }

    #[test]
    fn users_parses_colon_in_name() {
        let v = parse_users(&fixture("users.txt")).expect("该解析出来");
        let a = v.as_array().unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0]["id"], json!(0));
        assert_eq!(a[0]["名字"], json!("机主"));
        assert_eq!(a[0]["运行中"], json!(true));
    }

    #[test]
    fn display_takes_one_row_per_panel() {
        let v = parse_display(&fixture("display.txt")).expect("该解析出来");
        let a = v.as_array().unwrap();
        // 只认 DisplayDeviceInfo：mBaseDisplayInfo / mOverrideDisplayInfo 是同一块屏
        assert_eq!(a.len(), 1, "同一块屏不该出现多次：{v}");
        assert_eq!(a[0]["分辨率"], json!("1080x2424"));
        assert_eq!(a[0]["密度"], json!(420));
        assert_eq!(a[0]["刷新率"], json!("120Hz"));
    }

    #[test]
    fn packages_are_sorted_and_prefixed_stripped() {
        let v = parse_packages(&fixture("packages.txt")).expect("该解析出来");
        let a = v.as_array().unwrap();
        assert!(a.len() > 100);
        assert!(a
            .iter()
            .all(|x| !x.as_str().unwrap().starts_with("package:")));
        let names: Vec<&str> = a.iter().map(|x| x.as_str().unwrap()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "排过序");
    }

    #[test]
    fn package_info_picks_the_fields_that_matter() {
        let v = parse_package_info(&fixture("package-info.txt")).expect("该解析出来");
        assert_eq!(v["versionName"], json!("0.1.0"));
        assert_eq!(v["versionCode"], json!(1));
        assert_eq!(v["targetSdk"], json!(35));
        assert_eq!(v["minSdk"], json!(26));
        // 侧载的包 installerPackageName=null，不能原样印 "null"
        assert_eq!(v["安装者"], json!("（无）"));
        assert!(v["数据目录"].as_str().unwrap().contains("dev.vihv.agent"));
    }

    #[test]
    fn permissions_only_from_the_runtime_block() {
        let v = parse_permissions(&fixture("package-info.txt")).expect("该解析出来");
        let a = v.as_array().unwrap();
        assert!(!a.is_empty());
        // 全都有 `已授予` 字段：requested 段里的不算
        assert!(a.iter().all(|x| x.get("已授予").is_some()), "{v}");
        // 给了的排前面
        let first_granted = a[0]["已授予"].as_bool().unwrap();
        assert!(first_granted, "已授予的该排前面：{v}");
    }

    #[test]
    fn notifications_are_flat_records() {
        let v = parse_notifications(&fixture("notification.txt")).expect("该解析出来");
        let a = v.as_array().unwrap();
        assert!(!a.is_empty());
        assert!(a.iter().all(|x| x["包名"].is_string()), "{v}");
        // tag=null 不该出现（那是噪音）
        assert!(a.iter().all(|x| x["tag"] != json!("null")));
    }

    #[test]
    fn settings_list_is_key_value() {
        let v = parse_settings_list("a=1\nb=\nc=x y\n").expect("该解析出来");
        assert_eq!(v["a"], json!("1"));
        assert_eq!(v["b"], json!("")); // 空值保留成空串，不是丢掉
        assert_eq!(v["c"], json!("x y"));
        assert!(parse_settings_list("没有等号\n").is_none());
    }

    #[test]
    fn device_info_drops_the_attestation_noise() {
        let v = parse_device_info(&fixture("getprop.txt")).expect("该解析出来");
        // 就这几行，别把 getprop 那一屏照抄（实测原文 14 行、8 行是噪音）
        assert!(v.as_object().unwrap().len() <= 5, "{v}");
        assert_eq!(v["型号"], json!("Pixel 10"));
        assert_eq!(v["品牌"], json!("google"));
        assert_eq!(v["系统"], json!("17"));
        assert_eq!(v["SDK"], json!("37"));
        // `_for_attestation` 之类不该出现
        let s = v.to_string();
        assert!(!s.contains("attestation"), "{s}");
    }

    #[test]
    fn top_activity_expands_the_dot_shorthand() {
        let v = parse_top_activity(&fixture("activities.txt")).expect("该解析出来");
        let pkg = v["包名"].as_str().unwrap();
        let cls = v["页面"].as_str().unwrap();
        assert!(cls.starts_with(pkg), "`.MainActivity` 该补全成全名：{cls}");
        assert!(!cls.contains("/"), "{cls}");
    }

    #[test]
    fn every_parser_returns_none_on_garbage() {
        // 这条守的是「解析不出来就交回原文」那条规矩
        let junk = "随便一段不是 dumpsys 的东西\nfoo: bar\n";
        assert!(parse_battery(junk).is_none());
        assert!(parse_users(junk).is_none());
        assert!(parse_display(junk).is_none());
        assert!(parse_packages(junk).is_none());
        assert!(parse_notifications(junk).is_none());
        assert!(parse_top_activity(junk).is_none());
    }
}
