//! `agent weather` —— 天气（Open-Meteo，免 key）。
//!
//! **为什么在 Rust 这一侧、而不是像闹钟短信那样走 Kotlin**：它只是查网络，
//! 手机和电脑上**都能用** —— 放进「手机专有」那组，电脑上的 agent 就成了残废
//! （它常要查某个坐标的天气、或者给用户念一句今天的天气）。
//!
//! 数据源固定是 Open-Meteo：免 key、免注册，直接 GET。付费源（和风/彩云）要
//! key，而 key 在这台设备上没法安全地放着 —— 那属于用户自己配的东西，
//! 真需要再加。
//!
//! 坐标系是**十进制度**，和 `android-location` 的输出直接接得上：
//! `agent location current -q` 拿 latitude/longitude，喂给这里。

use serde_json::{json, Value};

/// Open-Meteo 的预报接口（免 key）。
const ENDPOINT: &str = "https://api.open-meteo.com/v1/forecast";

/// 一次请求最多等多久。手机上的网络说断就断，卡住不如报错。
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 逐小时最多要几个小时（Open-Meteo 免费额度按天算，别贪）。
const MAX_HOURS: u32 = 48;
/// 逐日最多几天。
const MAX_DAYS: u32 = 10;

pub const USAGE: &str = "\
用法：agent weather [current|hourly|daily|report] --lat L --lon N [--hours N] [--days N]
  current   现在（温度 / 体感 / 湿度 / 风 / 天气）
  hourly    逐小时（默认 24 小时，最多 48）
  daily     逐日（默认 7 天，最多 10）
  report    上面三个一起（默认）
  --lat --lon   十进制度（`agent location current -q` 里的 latitude / longitude）
坐标写反了会报错，不会拿上海的纬度当经度去查。";

/// 解析过的命令行。
#[derive(Debug, PartialEq)]
pub struct Args {
    pub sub: String,
    pub lat: f64,
    pub lon: f64,
    pub hours: u32,
    pub days: u32,
}

/// 解析 `argv`（**不含** `weather` 本身）。
pub fn parse_args(argv: &[String]) -> anyhow::Result<Args> {
    let mut sub = String::new();
    let mut lat: Option<f64> = None;
    let mut lon: Option<f64> = None;
    let mut hours: u32 = 24;
    let mut days: u32 = 7;

    let mut it = argv.iter().peekable();
    while let Some(a) = it.next() {
        let mut value = |name: &str| -> anyhow::Result<String> {
            it.next()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{name} 后面缺一个值"))
        };
        match a.as_str() {
            "current" | "hourly" | "daily" | "report" => sub = a.clone(),
            "--lat" => {
                lat = Some(value("--lat")?.parse().map_err(|_| {
                    anyhow::anyhow!("--lat 要一个数字（十进制度），给的是「{}」", a)
                })?)
            }
            "--lon" => {
                lon = Some(value("--lon")?.parse().map_err(|_| {
                    anyhow::anyhow!("--lon 要一个数字（十进制度），给的是「{}」", a)
                })?)
            }
            "--hours" => hours = value("--hours")?.parse()?,
            "--days" => days = value("--days")?.parse()?,
            "-h" | "--help" => anyhow::bail!("{USAGE}"),
            other => anyhow::bail!("不认识的参数：{other}\n{USAGE}"),
        }
    }

    // 坐标是这里最容易搞错的一件事（lat/lon 反了照样能查出一个结果，只是
    // 查的是地球另一边）。所以范围必须校验，宁可报错。
    let lat = lat.ok_or_else(|| anyhow::anyhow!("缺 --lat\n{USAGE}"))?;
    let lon = lon.ok_or_else(|| anyhow::anyhow!("缺 --lon\n{USAGE}"))?;
    if !(-90.0..=90.0).contains(&lat) {
        anyhow::bail!("--lat 要 -90..90，给的是 {lat}");
    }
    if !(-180.0..=180.0).contains(&lon) {
        anyhow::bail!("--lon 要 -180..180，给的是 {lon}");
    }

    Ok(Args {
        sub: if sub.is_empty() { "report".into() } else { sub },
        lat,
        lon,
        hours: hours.clamp(1, MAX_HOURS),
        days: days.clamp(1, MAX_DAYS),
    })
}

/// WMO 天气代码 → 中文。表来自 Open-Meteo 的文档（和各国气象台一致）。
pub fn condition(code: i64) -> &'static str {
    match code {
        0 => "晴",
        1 => "晴间多云",
        2 => "多云",
        3 => "阴",
        45 | 48 => "雾",
        51 | 53 | 55 => "毛毛雨",
        56 | 57 => "冻毛毛雨",
        61 => "小雨",
        63 => "中雨",
        65 => "大雨",
        66 | 67 => "冻雨",
        71 => "小雪",
        73 => "中雪",
        75 => "大雪",
        77 => "米雪",
        80 => "阵雨",
        81 => "中阵雨",
        82 => "强阵雨",
        85 | 86 => "阵雪",
        95 => "雷阵雨",
        96 | 99 => "雷暴伴冰雹",
        _ => "未知",
    }
}

/// 请求 URL。`timezone=auto` 让返回的时间就是当地时刻（省掉一层换算，
/// 也省掉用户看不懂的 UTC）。
fn url(a: &Args) -> String {
    let (current, hourly, daily) = match a.sub.as_str() {
        "current" => (
            "temperature_2m,relative_humidity_2m,apparent_temperature,is_day,precipitation,weather_code,wind_speed_10m",
            "",
            "",
        ),
        "hourly" => (
            "",
            "temperature_2m,precipitation_probability,weather_code",
            "",
        ),
        "daily" => (
            "",
            "",
            "weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max,sunrise,sunset",
        ),
        _ => (
            "temperature_2m,relative_humidity_2m,apparent_temperature,is_day,precipitation,weather_code,wind_speed_10m",
            "temperature_2m,precipitation_probability,weather_code",
            "weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max,sunrise,sunset",
        ),
    };
    let mut u = format!(
        "{ENDPOINT}?latitude={}&longitude={}&timezone=auto",
        a.lat, a.lon
    );
    if !current.is_empty() {
        u.push_str(&format!("&current={current}"));
    }
    if !hourly.is_empty() {
        u.push_str(&format!("&hourly={hourly}&forecast_hours={}", a.hours));
    }
    if !daily.is_empty() {
        u.push_str(&format!("&daily={daily}&forecast_days={}", a.days));
    }
    u
}

/// 跑一次（网络 + 组装）。
pub async fn run(argv: &[String]) -> anyhow::Result<Value> {
    let args = parse_args(argv)?;
    let client = crate::llm::shared_http()?;
    let resp = client
        .get(url(&args))
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("连不上 Open-Meteo：{e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "Open-Meteo 返回 {status}：{}",
            text.chars().take(200).collect::<String>()
        );
    }
    let raw: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("Open-Meteo 回的 JSON 读不了：{e}"))?;
    shape(&args, &raw)
}

/// 把 Open-Meteo 的原始响应整理成我们要的形状。**纯函数**，单测就测它。
fn shape(a: &Args, raw: &Value) -> anyhow::Result<Value> {
    // 两种错形状都见过：`error:true` + `reason`（HTTP 200 的"参数不对"），
    // 以及直接一个 `error` 字段。reason 带上比只回一个 `true` 有用得多。
    let reason = raw.get("reason").and_then(|r| r.as_str()).unwrap_or("");
    if raw.get("error").is_some_and(|e| !e.is_null()) {
        if !reason.is_empty() {
            anyhow::bail!("Open-Meteo 拒绝了这次请求：{reason}");
        }
        anyhow::bail!("Open-Meteo 报错：{}", raw.get("error").unwrap());
    }
    if !reason.is_empty() {
        anyhow::bail!("Open-Meteo 拒绝了这次请求：{reason}");
    }

    let mut data = json!({
        "place": {
            "latitude": raw.get("latitude").cloned().unwrap_or(Value::Null),
            "longitude": raw.get("longitude").cloned().unwrap_or(Value::Null),
            "timezone": raw.get("timezone").cloned().unwrap_or(Value::Null),
        }
    });

    if a.sub == "current" || a.sub == "report" {
        if let Some(c) = raw.get("current") {
            let code = c.get("weather_code").and_then(|v| v.as_i64()).unwrap_or(-1);
            data["current"] = json!({
                "time": c.get("time").cloned().unwrap_or(Value::Null),
                "condition": condition(code),
                "code": code,
                "temperature_c": c.get("temperature_2m").cloned().unwrap_or(Value::Null),
                "feels_like_c": c.get("apparent_temperature").cloned().unwrap_or(Value::Null),
                "humidity_pct": c.get("relative_humidity_2m").cloned().unwrap_or(Value::Null),
                "precipitation_mm": c.get("precipitation").cloned().unwrap_or(Value::Null),
                "wind_kmh": c.get("wind_speed_10m").cloned().unwrap_or(Value::Null),
                "is_day": c.get("is_day").cloned().unwrap_or(Value::Null),
            });
        }
    }

    if a.sub == "hourly" || a.sub == "report" {
        data["hourly"] = zip_time(raw, "hourly", &[
            ("temperature_2m", "temperature_c"),
            ("precipitation_probability", "precipitation_probability_pct"),
            ("weather_code", "code"),
        ])?;
    }

    if a.sub == "daily" || a.sub == "report" {
        data["daily"] = zip_time(raw, "daily", &[
            ("weather_code", "code"),
            ("temperature_2m_max", "high_c"),
            ("temperature_2m_min", "low_c"),
            ("precipitation_probability_max", "precipitation_probability_pct"),
            ("sunrise", "sunrise"),
            ("sunset", "sunset"),
        ])?;
        // daily 那段补一个中文的天气描述：模型和界面都要用，别让它自己拿 code 去查表
        if let Some(arr) = data["daily"].as_array_mut() {
            for row in arr {
                let code = row.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
                row["condition"] = json!(condition(code));
            }
        }
    }

    Ok(json!({ "ok": true, "data": data }))
}

/// Open-Meteo 的时段数据是「一列字段 = 一个数组」的平铺结构，
/// 这里按 `time` 的长度把它拧成一条条记录（模型按行读，比按列读省事）。
fn zip_time(raw: &Value, section: &str, fields: &[(&str, &str)]) -> anyhow::Result<Value> {
    let block = match raw.get(section) {
        Some(b) => b,
        // 只问 current 时接口不会带这一段 —— 不是错误
        None => return Ok(json!([])),
    };
    let times = block
        .get("time")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();

    let out: Vec<Value> = times
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut row = json!({ "time": t });
            for (from, to) in fields {
                let v = block
                    .get(*from)
                    .and_then(|a| a.as_array())
                    .and_then(|a| a.get(i))
                    .cloned()
                    .unwrap_or(Value::Null);
                row[*to] = v;
            }
            row
        })
        .collect();
    Ok(Value::Array(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Args {
        parse_args(&s.iter().map(|x| x.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn subcommand_defaults_to_report() {
        let a = args(&["--lat", "31.2", "--lon", "121.4"]);
        assert_eq!(a.sub, "report");
        assert_eq!(a.hours, 24);
        assert_eq!(a.days, 7);
    }

    /// 纬度 -90..90、经度 -180..180：写反了要报错，不能"照样查出一个结果"。
    #[test]
    fn coordinates_are_range_checked_and_required() {
        let e = parse_args(&["--lat".into(), "121".into(), "--lon".into(), "31".into()])
            .unwrap_err();
        assert!(e.to_string().contains("-90..90"), "{e}");

        let e = parse_args(&["--lat".into(), "31".into()]).unwrap_err();
        assert!(e.to_string().contains("--lon"), "{e}");

        let e = parse_args(&["--lat".into(), "x".into(), "--lon".into(), "1".into()]).unwrap_err();
        assert!(e.to_string().contains("数字"), "{e}");
    }

    #[test]
    fn hours_and_days_are_clamped_to_what_the_api_allows() {
        assert_eq!(args(&["--lat", "1", "--lon", "1", "--hours", "999"]).hours, MAX_HOURS);
        assert_eq!(args(&["--lat", "1", "--lon", "1", "--days", "99"]).days, MAX_DAYS);
        assert_eq!(args(&["--lat", "1", "--lon", "1", "--hours", "0"]).hours, 1);
    }

    #[test]
    fn the_url_asks_only_for_what_the_subcommand_shows() {
        let u = url(&args(&["current", "--lat", "31.2", "--lon", "121.4"]));
        assert!(u.contains("&current="), "{u}");
        assert!(!u.contains("&hourly="), "只要 current 就别要 hourly：{u}");
        assert!(!u.contains("&daily="), "{u}");
        assert!(u.contains("timezone=auto"), "返回的时间得是当地时间：{u}");

        let u = url(&args(&["daily", "--lat", "1", "--lon", "2", "--days", "3"]));
        assert!(u.contains("forecast_days=3"), "{u}");
    }

    #[test]
    fn wmo_codes_have_chinese_conditions() {
        assert_eq!(condition(0), "晴");
        assert_eq!(condition(95), "雷阵雨");
        assert_eq!(condition(1234), "未知");
    }

    /// 平铺的列 → 一行行记录，并且每行都带中文天气。
    #[test]
    fn hourly_and_daily_become_rows() {
        let raw = json!({
            "latitude": 31.25, "longitude": 121.5, "timezone": "Asia/Shanghai",
            "current": {
                "time": "2026-02-05T10:00", "temperature_2m": 8.4, "apparent_temperature": 6.1,
                "relative_humidity_2m": 62, "precipitation": 0.0, "weather_code": 3,
                "wind_speed_10m": 11.2, "is_day": 1
            },
            "hourly": {
                "time": ["2026-02-05T10:00", "2026-02-05T11:00"],
                "temperature_2m": [8.4, 9.1],
                "precipitation_probability": [0, 20],
                "weather_code": [3, 61]
            },
            "daily": {
                "time": ["2026-02-05"],
                "weather_code": [61],
                "temperature_2m_max": [10.0],
                "temperature_2m_min": [4.0],
                "precipitation_probability_max": [70],
                "sunrise": ["2026-02-05T06:41"],
                "sunset": ["2026-02-05T17:29"]
            }
        });

        let out = shape(&args(&["report", "--lat", "31.25", "--lon", "121.5"]), &raw).unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["data"]["place"]["timezone"], "Asia/Shanghai");
        assert_eq!(out["data"]["current"]["condition"], "阴");
        assert_eq!(out["data"]["current"]["temperature_c"], 8.4);
        assert_eq!(out["data"]["hourly"][1]["temperature_c"], 9.1);
        assert_eq!(out["data"]["hourly"][1]["precipitation_probability_pct"], 20);
        assert_eq!(out["data"]["daily"][0]["condition"], "小雨");
        assert_eq!(out["data"]["daily"][0]["high_c"], 10.0);
        assert_eq!(out["data"]["daily"][0]["sunset"], "2026-02-05T17:29");
    }

    #[test]
    fn missing_sections_are_empty_not_an_error() {
        let raw = json!({"latitude": 1.0, "longitude": 2.0, "timezone": "UTC"});
        let out = shape(&args(&["report", "--lat", "1", "--lon", "2"]), &raw).unwrap();
        assert_eq!(out["data"]["hourly"], json!([]));
        assert_eq!(out["data"]["daily"], json!([]));
        assert!(out["data"].get("current").is_none());
    }

    #[test]
    fn api_errors_are_surfaced() {
        let raw = json!({"error": true, "reason": "Latitude must be in range"});
        let e = shape(&args(&["current", "--lat", "1", "--lon", "2"]), &raw).unwrap_err();
        assert!(e.to_string().contains("Latitude must be in range"), "{e}");
    }

    /// 帮助文本要跟着实际支持的东西走（不然模型照 help 用会撞墙）。
    #[test]
    fn usage_mentions_every_subcommand() {
        for s in ["current", "hourly", "daily", "report", "--lat", "--lon", "--hours", "--days"] {
            assert!(USAGE.contains(s), "用法里少了 {s}");
        }
    }
}
