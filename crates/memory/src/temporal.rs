//! P1 检索质量：自然语言时间解析（直译 mnemopi `core/temporal-parser.ts`）。
//!
//! 从查询中解析出“事件日期”（UTC 午夜毫秒），供召回打分切换时间信号为
//! 相对目标日期的指数衰减。支持：ISO/斜杠日期、`january 5`、today/yesterday/
//! tomorrow、`last/this/next monday`、`last/this/next week|month|year`、
//! `3 days ago`、`in 5 weeks`、recently/vague 相对词。
//!
//! 说明：模式为英文（与 mnemopi 一致）；中文时间表达（“上周/三天前”）不在
//! 本次范围，后续可加。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use regex::Regex;

/// 已解析的自然语言日期。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDate {
    /// UTC 午夜纪元毫秒（对齐记录 ts 单位）。
    pub epoch_ms: u64,
    /// 精度：day/week/month/year/relative（对应 mnemopi DatePrecision）。
    pub precision: &'static str,
    /// 时间标签（ISO 日期、`week-N-YYYY`、星期名、`this-week` 等）。
    pub tags: Vec<String>,
}

/// 从文本提取的时间信息（对齐 mnemopi `TemporalInfo`）。
#[derive(Debug, Clone, Default)]
pub struct TemporalInfo {
    pub event_date_ms: Option<u64>,
    pub event_date_precision: &'static str,
    pub temporal_tags: Vec<String>,
    pub primary_signal: Option<String>,
}

const MS_PER_DAY: i64 = 86_400_000;

const DAY_MAP: &[(&str, i64)] = &[
    ("monday", 0), ("tuesday", 1), ("wednesday", 2), ("thursday", 3),
    ("friday", 4), ("saturday", 5), ("sunday", 6),
    ("mon", 0), ("tue", 1), ("wed", 2), ("thu", 3), ("fri", 4), ("sat", 5), ("sun", 6),
];

const MONTH_MAP: &[(&str, i64)] = &[
    ("january", 1), ("february", 2), ("march", 3), ("april", 4), ("may", 5),
    ("june", 6), ("july", 7), ("august", 8), ("september", 9), ("october", 10),
    ("november", 11), ("december", 12),
    ("jan", 1), ("feb", 2), ("mar", 3), ("apr", 4), ("jun", 6), ("jul", 7),
    ("aug", 8), ("sep", 9), ("oct", 10), ("nov", 11), ("dec", 12),
];

const DAY_NAMES: &[&str] = &[
    "sunday", "monday", "tuesday", "wednesday", "thursday", "friday", "saturday",
];

const NAMED_TIME_KEYS: &[&str] = &[
    "morning", "afternoon", "evening", "night", "midnight", "noon", "dawn", "dusk",
];

/// 日期（y,m,d）→ 自 1970-01-01 的天数（Howard Hinnant `days_from_civil`）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 天数 → 日期（y,m,d）。
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn date_utc(y: i64, m: i64, d: i64) -> Option<i64> {
    let days = days_from_civil(y, m, d);
    let (cy, cm, cd) = civil_from_days(days);
    if cy == y && cm == m && cd == d {
        Some(days * MS_PER_DAY)
    } else {
        None
    }
}

fn add_days(epoch_ms: i64, days: i64) -> i64 {
    epoch_ms + days * MS_PER_DAY
}

fn date_only(epoch_ms: i64) -> i64 {
    let days = epoch_ms.div_euclid(MS_PER_DAY);
    days * MS_PER_DAY
}

fn iso_date(epoch_ms: i64) -> String {
    let days = epoch_ms / MS_PER_DAY;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// 星期几（周一=0 … 周日=6，对齐 Python weekday）。
fn weekday(epoch_ms: i64) -> i64 {
    let days = epoch_ms / MS_PER_DAY;
    // 1970-01-01 是周四（=3）
    (days + 3).rem_euclid(7)
}

fn day_name(epoch_ms: i64) -> &'static str {
    // weekday 结果恒为 0..=6，cast 安全
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let idx = weekday(epoch_ms) as usize;
    DAY_NAMES[idx]
}

fn iso_week(epoch_ms: i64) -> i64 {
    let d = date_only(epoch_ms);
    // 周四在的那一周
    let thursday = add_days(d, 4 - (weekday(d) + 6) % 7);
    let (y, _, _) = civil_from_days(thursday / MS_PER_DAY);
    let jan1 = date_utc(y, 1, 1).expect("合法日期");
    // div_ceil 语义（除数 7，被除数非负）：(x + 6) / 7
    (thursday - jan1) / MS_PER_DAY / 7 + 1
}

fn tags_for_day(epoch_ms: i64) -> Vec<String> {
    let (y, _, _) = civil_from_days(epoch_ms / MS_PER_DAY);
    vec![
        iso_date(epoch_ms),
        format!("week-{}-{y}", iso_week(epoch_ms)),
        day_name(epoch_ms).to_string(),
    ]
}

fn resolve_relative_day(reference: i64, day_name_text: &str, qualifier: &str) -> i64 {
    let target_wd = DAY_MAP
        .iter()
        .find(|(name, _)| *name == day_name_text)
        .map_or_else(|| weekday(reference), |(_, wd)| *wd);
    let current_wd = weekday(reference);
    let base = date_only(reference);
    match qualifier {
        "last" => add_days(base, -(((current_wd - target_wd).rem_euclid(7)) + 7)),
        "next" => {
            let mut diff = (target_wd - current_wd).rem_euclid(7);
            if diff == 0 {
                diff = 7;
            }
            add_days(base, diff)
        }
        _ => add_days(base, -((current_wd - target_wd).rem_euclid(7))),
    }
}

fn delta_date(reference: i64, num: i64, unit: &str, direction: i64) -> Option<i64> {
    let days = match unit {
        "second" | "minute" | "hour" => {
            let seconds = match unit {
                "second" => num,
                "minute" => num * 60,
                _ => num * 3600,
            };
            return Some(reference + direction * seconds * 1000);
        }
        "day" => num,
        "week" => num * 7,
        "month" => num * 30,
        "year" => num * 365,
        _ => return None,
    };
    Some(add_days(reference, direction * days))
}

/// 静态正则缓存：文本 → 已编译 &'static Regex（数量固定，leak 可忽略）。
fn re(text: &str) -> &'static Regex {
    static RE_CACHE: LazyLock<Mutex<HashMap<String, &'static Regex>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut cache = RE_CACHE.lock().expect("正则缓存锁可获取");
    if let Some(r) = cache.get(text) {
        return r;
    }
    let compiled: &'static Regex =
        Box::leak(Box::new(Regex::new(text).expect("静态正则合法")));
    cache.insert(text.to_string(), compiled);
    compiled
}

/// 解析文本中的自然语言日期；无匹配返回 None。
/// `reference`：参考时刻（默认 now，毫秒）。
///
/// # Panics
/// 内建正则编译失败时（静态模式，正常不会发生）。
#[must_use]
#[allow(
    clippy::too_many_lines,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap // reference 为 u64（≥ 1970 后），i64 足够容纳；仅日期运算内部使用
)]
pub fn parse_nl_date(text: &str, reference: Option<u64>) -> Option<ParsedDate> {
    let reference = reference.unwrap_or_else(now_ms);
    let text_lower = text.to_ascii_lowercase();

    // ISO `2025-03-01`
    if let Some(caps) = re(r"\b(\d{4})-(\d{2})-(\d{2})\b").captures(text) {
        let y = caps[1].parse::<i64>().ok()?;
        let m = caps[2].parse::<i64>().ok()?;
        let d = caps[3].parse::<i64>().ok()?;
        if let Some(ms) = date_utc(y, m, d) {
            return Some(ParsedDate { epoch_ms: ms as u64, precision: "day", tags: tags_for_day(ms) });
        }
    }

    // 斜杠 `3/5/2024` / `03/05/24`（日 > 12 时按日/月/年，否则月/日/年）
    if let Some(caps) = re(r"\b(\d{1,2})/(\d{1,2})/(\d{2,4})\b").captures(text) {
        let a = caps[1].parse::<i64>().ok()?;
        let b = caps[2].parse::<i64>().ok()?;
        let mut y = caps[3].parse::<i64>().ok()?;
        if y < 100 {
            y += 2000;
        }
        let ms = if a > 12 { date_utc(y, b, a) } else { date_utc(y, a, b) };
        if let Some(ms) = ms {
            return Some(ParsedDate { epoch_ms: ms as u64, precision: "day", tags: tags_for_day(ms) });
        }
    }

    // `january 5` / `jan 5th, 2024`
    if let Some(caps) = re(r"\b(january|february|march|april|may|june|july|august|september|october|november|december|jan|feb|mar|apr|jun|jul|aug|sep|oct|nov|dec)\s+(\d{1,2})(?:st|nd|rd|th)?(?:,?\s*(\d{4}))?\b")
        .captures(&text_lower)
    {
        let month = MONTH_MAP.iter().find(|(name, _)| *name == &caps[1]).map(|(_, m)| *m)?;
        let day = caps[2].parse::<i64>().ok()?;
        let year = caps
            .get(3)
            .map_or(reference as i64, |g| g.as_str().parse::<i64>().unwrap_or(reference as i64));
        if let Some(ms) = date_utc(year, month, day) {
            return Some(ParsedDate { epoch_ms: ms as u64, precision: "day", tags: tags_for_day(ms) });
        }
    }

    if re(r"\btoday\b").is_match(&text_lower) {
        let d = date_only(reference as i64);
        return Some(ParsedDate {
            epoch_ms: d as u64,
            precision: "day",
            tags: vec![iso_date(d), day_name(d).to_string()],
        });
    }

    if re(r"\byesterday\b").is_match(&text_lower) {
        let d = add_days(date_only(reference as i64), -1);
        return Some(ParsedDate {
            epoch_ms: d as u64,
            precision: "day",
            tags: vec![iso_date(d), day_name(d).to_string(), "yesterday".to_string()],
        });
    }

    if re(r"\btomorrow\b").is_match(&text_lower) {
        let d = add_days(date_only(reference as i64), 1);
        return Some(ParsedDate {
            epoch_ms: d as u64,
            precision: "day",
            tags: vec![iso_date(d), day_name(d).to_string(), "tomorrow".to_string()],
        });
    }

    if re(r"\bday\s+before\s+yesterday\b").is_match(&text_lower) {
        let d = add_days(date_only(reference as i64), -2);
        return Some(ParsedDate { epoch_ms: d as u64, precision: "day", tags: vec![iso_date(d)] });
    }

    // `last|this|next monday`
    if let Some(caps) = re(r"\b(last|this|next)\s+(monday|tuesday|wednesday|thursday|friday|saturday|sunday|mon|tue|wed|thu|fri|sat|sun)\b")
        .captures(&text_lower)
    {
        let qualifier = &caps[1];
        let day_name_text = &caps[2];
        let d = resolve_relative_day(reference as i64, day_name_text, qualifier);
        return Some(ParsedDate {
            epoch_ms: d as u64,
            precision: "day",
            tags: vec![
                iso_date(d),
                format!("week-{}-{}", iso_week(d), civil_from_days(d / MS_PER_DAY).0),
                day_name_text.to_string(),
                qualifier.to_string(),
            ],
        });
    }

    // `(on )?monday`
    if let Some(caps) = re(r"\b(on\s+)?(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b")
        .captures(&text_lower)
    {
        let d = resolve_relative_day(reference as i64, &caps[2], "this");
        return Some(ParsedDate {
            epoch_ms: d as u64,
            precision: "day",
            tags: vec![
                iso_date(d),
                format!("week-{}-{}", iso_week(d), civil_from_days(d / MS_PER_DAY).0),
                caps[2].to_string(),
            ],
        });
    }

    // `this|last|next week|month|year`
    if let Some(caps) = re(r"\b(this|last|next)\s+(week|month|year)\b").captures(&text_lower) {
        let qualifier = &caps[1];
        let unit = &caps[2];
        let ref_date = date_only(reference as i64);
        match qualifier {
            "this" => match unit {
                "week" => {
                    return Some(ParsedDate {
                        epoch_ms: ref_date as u64,
                        precision: "week",
                        tags: vec![
                            format!("week-{}-{}", iso_week(ref_date), civil_from_days(ref_date / MS_PER_DAY).0),
                            "this-week".to_string(),
                        ],
                    });
                }
                "month" => {
                    let (y, m, _) = civil_from_days(ref_date / MS_PER_DAY);
                    return Some(ParsedDate {
                        epoch_ms: ref_date as u64,
                        precision: "month",
                        tags: vec![format!("{y:04}-{m:02}"), "this-month".to_string()],
                    });
                }
                _ => {
                    let (y, _, _) = civil_from_days(ref_date / MS_PER_DAY);
                    return Some(ParsedDate {
                        epoch_ms: ref_date as u64,
                        precision: "year",
                        tags: vec![format!("{y:04}"), "this-year".to_string()],
                    });
                }
            },
            "last" => match unit {
                "week" => {
                    let d = add_days(ref_date, -7);
                    return Some(ParsedDate {
                        epoch_ms: d as u64,
                        precision: "week",
                        tags: vec![
                            format!("week-{}-{}", iso_week(d), civil_from_days(d / MS_PER_DAY).0),
                            "last-week".to_string(),
                        ],
                    });
                }
                "month" => {
                    let (y, m, _) = civil_from_days(ref_date / MS_PER_DAY);
                    let (ny, nm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
                    let d = date_utc(ny, nm, 1)?;
                    return Some(ParsedDate {
                        epoch_ms: d as u64,
                        precision: "month",
                        tags: vec![format!("{ny:04}-{nm:02}"), "last-month".to_string()],
                    });
                }
                _ => {
                    let (y, _, _) = civil_from_days(ref_date / MS_PER_DAY);
                    let d = date_utc(y - 1, 1, 1)?;
                    return Some(ParsedDate {
                        epoch_ms: d as u64,
                        precision: "year",
                        tags: vec![format!("{:04}", y - 1), "last-year".to_string()],
                    });
                }
            },
            _ => match unit {
                "week" => {
                    let d = add_days(ref_date, 7);
                    return Some(ParsedDate {
                        epoch_ms: d as u64,
                        precision: "week",
                        tags: vec![
                            format!("week-{}-{}", iso_week(d), civil_from_days(d / MS_PER_DAY).0),
                            "next-week".to_string(),
                        ],
                    });
                }
                "month" => {
                    let (y, m, _) = civil_from_days(ref_date / MS_PER_DAY);
                    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
                    let d = date_utc(ny, nm, 1)?;
                    return Some(ParsedDate {
                        epoch_ms: d as u64,
                        precision: "month",
                        tags: vec![format!("{ny:04}-{nm:02}"), "next-month".to_string()],
                    });
                }
                _ => {
                    let (y, _, _) = civil_from_days(ref_date / MS_PER_DAY);
                    let d = date_utc(y + 1, 1, 1)?;
                    return Some(ParsedDate {
                        epoch_ms: d as u64,
                        precision: "year",
                        tags: vec![format!("{:04}", y + 1), "next-year".to_string()],
                    });
                }
            },
        }
    }

    // `3 days ago` / `5 weeks back`
    if let Some(caps) = re(r"\b(\d+)\s+(second|minute|hour|day|week|month|year)s?\s+(ago|before|earlier|back)\b")
        .captures(&text_lower)
    {
        let num = caps[1].parse::<i64>().ok()?;
        let unit = &caps[2];
        let d = delta_date(reference as i64, num, unit, -1)?;
        let precision = if unit == "day" || unit == "hour" { "day" } else { "week" };
        return Some(ParsedDate {
            epoch_ms: d as u64,
            precision,
            tags: vec![iso_date(d), format!("{num}-{unit}s-ago")],
        });
    }

    // `in 5 weeks`
    if let Some(caps) = re(r"\bin\s+(\d+)\s+(second|minute|hour|day|week|month|year)s?\b")
        .captures(&text_lower)
    {
        let num = caps[1].parse::<i64>().ok()?;
        let unit = &caps[2];
        let d = delta_date(reference as i64, num, unit, 1)?;
        let precision = if unit == "day" || unit == "hour" { "day" } else { "week" };
        return Some(ParsedDate {
            epoch_ms: d as u64,
            precision,
            tags: vec![iso_date(d), format!("in-{num}-{unit}s")],
        });
    }

    if re(r"\b(recently|lately|not long ago)\b").is_match(&text_lower) {
        let d = date_only(reference as i64);
        return Some(ParsedDate {
            epoch_ms: d as u64,
            precision: "relative",
            tags: vec!["recently".to_string()],
        });
    }

    if re(r"\b(a while ago|some time ago|long ago)\b").is_match(&text_lower) {
        let d = date_only(reference as i64);
        return Some(ParsedDate {
            epoch_ms: d as u64,
            precision: "relative",
            tags: vec!["vague".to_string()],
        });
    }

    None
}

/// 提取文本中的时间信息（对齐 mnemopi `extractTemporal`）。
#[must_use]
pub fn extract_temporal(text: &str, reference: Option<u64>) -> TemporalInfo {
    let parsed = parse_nl_date(text, reference);
    let mut tags: Vec<String> = Vec::new();
    let text_lower = text.to_ascii_lowercase();
    for time_name in NAMED_TIME_KEYS {
        if text_lower.contains(time_name) {
            tags.push((*time_name).to_string());
            break;
        }
    }
    let Some(parsed) = parsed else {
        return TemporalInfo {
            event_date_ms: None,
            event_date_precision: "unknown",
            temporal_tags: tags.clone(),
            primary_signal: tags.first().cloned(),
        };
    };
    let mut all_tags = parsed.tags;
    all_tags.extend(tags);
    TemporalInfo {
        event_date_ms: Some(parsed.epoch_ms),
        event_date_precision: parsed.precision,
        primary_signal: all_tags.first().cloned(),
        temporal_tags: all_tags,
    }
}

/// 记录时间戳相对目标时间的指数衰减（对齐 mnemopi `temporalBoost`：
/// 过去的记录随时间衰减趋 0，当天/未来记录满值 1.0）。
#[must_use]
#[allow(clippy::cast_precision_loss)] // 毫秒级距离 / 小时换算，精度损失无实际影响
pub fn temporal_boost(ts_ms: u64, query_time_ms: u64, halflife_hours: f64) -> f64 {
    let distance_hours = query_time_ms.saturating_sub(ts_ms) as f64 / 3_600_000.0;
    (-distance_hours / halflife_hours.max(0.001)).exp()
}

fn now_ms() -> u64 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // u128→u64 截断在毫秒量级可接受
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定参考：2025-03-05（周三）12:00 UTC。
    #[allow(clippy::cast_sign_loss)] // 日期恒在 1970 后
    fn ref_ms() -> u64 {
        date_utc(2025, 3, 5).unwrap() as u64 + 12 * 3_600_000
    }

    #[allow(clippy::cast_sign_loss)] // 日期恒在 1970 后
    fn day(y: i64, m: i64, d: i64) -> u64 {
        date_utc(y, m, d).unwrap() as u64
    }

    #[test]
    fn iso_date_parsed() {
        let p = parse_nl_date("参考 2024-06-15 的配置", Some(ref_ms())).unwrap();
        assert_eq!(p.epoch_ms, day(2024, 6, 15));
        assert_eq!(p.precision, "day");
        assert!(p.tags[0].starts_with("2024-06-15"));
    }

    #[test]
    fn slash_date_parsed() {
        let p = parse_nl_date("7/4/2023", Some(ref_ms())).unwrap();
        assert_eq!(p.epoch_ms, day(2023, 7, 4));
        // 日 > 12 → 日/月/年
        let p2 = parse_nl_date("14/3/2023", Some(ref_ms())).unwrap();
        assert_eq!(p2.epoch_ms, day(2023, 3, 14));
    }

    #[test]
    fn yesterday_today_tomorrow() {
        assert_eq!(parse_nl_date("yesterday", Some(ref_ms())).unwrap().epoch_ms, day(2025, 3, 4));
        assert_eq!(parse_nl_date("today", Some(ref_ms())).unwrap().epoch_ms, day(2025, 3, 5));
        assert_eq!(parse_nl_date("tomorrow", Some(ref_ms())).unwrap().epoch_ms, day(2025, 3, 6));
    }

    #[test]
    fn relative_weekday() {
        // 2025-03-05 是周三；this monday → 2025-03-03
        let p = parse_nl_date("this monday", Some(ref_ms())).unwrap();
        assert_eq!(p.epoch_ms, day(2025, 3, 3));
        // last friday = this friday 再回 7 天 → 2025-02-21（对齐 omp："last" 保证回退一整周）
        let p2 = parse_nl_date("last friday", Some(ref_ms())).unwrap();
        assert_eq!(p2.epoch_ms, day(2025, 2, 21));
        // next monday → 2025-03-10
        let p3 = parse_nl_date("next monday", Some(ref_ms())).unwrap();
        assert_eq!(p3.epoch_ms, day(2025, 3, 10));
    }

    #[test]
    fn last_month_and_week() {
        let p = parse_nl_date("last month", Some(ref_ms())).unwrap();
        assert_eq!(p.epoch_ms, day(2025, 2, 1));
        assert_eq!(p.precision, "month");
        let p2 = parse_nl_date("last week", Some(ref_ms())).unwrap();
        assert_eq!(p2.epoch_ms, day(2025, 2, 26)); // 周三 - 7 天
    }

    #[test]
    fn days_ago() {
        // 相对时刻保留时分（对齐 omp `deltaDate`：不裁剪到午夜）
        let p = parse_nl_date("3 days ago", Some(ref_ms())).unwrap();
        assert_eq!(p.epoch_ms, day(2025, 3, 2) + 12 * 3_600_000);
        let p2 = parse_nl_date("2 weeks ago", Some(ref_ms())).unwrap();
        assert_eq!(p2.epoch_ms, day(2025, 2, 19) + 12 * 3_600_000);
    }

    #[test]
    fn recently_relative() {
        let p = parse_nl_date("what changed recently?", Some(ref_ms())).unwrap();
        assert_eq!(p.precision, "relative");
        assert_eq!(p.epoch_ms, day(2025, 3, 5));
    }

    #[test]
    fn no_match_returns_none() {
        assert!(parse_nl_date("如何优化构建时间", Some(ref_ms())).is_none());
    }

    #[test]
    fn extract_temporal_tags_named_time() {
        let info = extract_temporal("deployed yesterday morning", Some(ref_ms()));
        assert_eq!(info.event_date_ms, Some(day(2025, 3, 4)));
        assert!(info.temporal_tags.iter().any(|t| t == "morning"));
        assert_eq!(info.primary_signal.as_deref(), Some("2025-03-04"));
    }

    #[test]
    fn temporal_boost_decays_past_only() {
        // 目标时刻前 48h → e^(-48/336) ≈ 0.867
        let boost = temporal_boost(day(2025, 3, 3), day(2025, 3, 5), 336.0);
        assert!((boost - (-48.0f64 / 336.0).exp()).abs() < 1e-9);
        // 未来记录 → 满值 1.0（对齐 omp `Math.max(0, …)` 语义）
        let future = temporal_boost(day(2025, 3, 10), day(2025, 3, 5), 336.0);
        assert!((future - 1.0).abs() < 1e-12);
    }

    #[test]
    fn leap_year_roundtrip() {
        let ms = day(2024, 2, 29);
        let p = parse_nl_date("2024-02-29", Some(ref_ms())).unwrap();
        assert_eq!(p.epoch_ms, ms);
        // 非法日期（2023 无 2 月 29）→ 无匹配
        assert!(parse_nl_date("2023-02-29", Some(ref_ms())).is_none());
    }
}
