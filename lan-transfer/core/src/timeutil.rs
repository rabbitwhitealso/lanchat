//! 时间工具：毫秒时间戳、本地时区转换与显示（不依赖 chrono，直接调用系统 C 库）

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Local {
    pub year: i32,
    pub mon: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
}

/// 从 1970-01-01 起的天数 → (年, 月, 日)。Howard Hinnant 的 civil_from_days 算法
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

pub fn utc(ts_ms: u64) -> Local {
    let secs = (ts_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u32;
    let (year, mon, day) = civil_from_days(days);
    Local { year, mon, day, hour: rem / 3600, min: rem % 3600 / 60, sec: rem % 60 }
}

#[cfg(windows)]
pub fn local(ts_ms: u64) -> Local {
    #[repr(C)]
    struct Tm {
        sec: i32,
        min: i32,
        hour: i32,
        mday: i32,
        mon: i32,
        year: i32,
        wday: i32,
        yday: i32,
        isdst: i32,
    }
    extern "C" {
        fn _localtime64_s(tm: *mut Tm, time: *const i64) -> i32;
    }
    let t = (ts_ms / 1000) as i64;
    let mut tm = Tm { sec: 0, min: 0, hour: 0, mday: 0, mon: 0, year: 0, wday: 0, yday: 0, isdst: 0 };
    let rc = unsafe { _localtime64_s(&mut tm, &t) };
    if rc != 0 {
        return utc(ts_ms);
    }
    Local {
        year: tm.year + 1900,
        mon: (tm.mon + 1) as u32,
        day: tm.mday as u32,
        hour: tm.hour as u32,
        min: tm.min as u32,
        sec: tm.sec as u32,
    }
}

#[cfg(not(windows))]
pub fn local(ts_ms: u64) -> Local {
    // glibc / musl 在 64 位 Linux 上的 struct tm 布局
    #[repr(C)]
    struct Tm {
        sec: i32,
        min: i32,
        hour: i32,
        mday: i32,
        mon: i32,
        year: i32,
        wday: i32,
        yday: i32,
        isdst: i32,
        gmtoff: i64,
        zone: *const std::ffi::c_char,
    }
    extern "C" {
        fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
    }
    let t = (ts_ms / 1000) as i64;
    let mut tm = Tm {
        sec: 0,
        min: 0,
        hour: 0,
        mday: 0,
        mon: 0,
        year: 0,
        wday: 0,
        yday: 0,
        isdst: 0,
        gmtoff: 0,
        zone: std::ptr::null(),
    };
    let p = unsafe { localtime_r(&t, &mut tm) };
    if p.is_null() {
        return utc(ts_ms);
    }
    Local {
        year: tm.year + 1900,
        mon: (tm.mon + 1) as u32,
        day: tm.mday as u32,
        hour: tm.hour as u32,
        min: tm.min as u32,
        sec: tm.sec as u32,
    }
}

/// 聊天气泡上的时间：今天只显示 "14:32"，今年其它日子 "10-14 09:12"，更早 "2025-12-31 09:12"
pub fn format_ts(ts_ms: u64, now_ms: u64) -> String {
    let t = local(ts_ms);
    let n = local(now_ms);
    if (t.year, t.mon, t.day) == (n.year, n.mon, n.day) {
        format!("{:02}:{:02}", t.hour, t.min)
    } else if t.year == n.year {
        format!("{:02}-{:02} {:02}:{:02}", t.mon, t.day, t.hour, t.min)
    } else {
        format!("{}-{:02}-{:02} {:02}:{:02}", t.year, t.mon, t.day, t.hour, t.min)
    }
}

/// 日志用的完整时间 "2026-10-15 14:03:22"
pub fn format_full(ts_ms: u64) -> String {
    let t = local(ts_ms);
    format!("{}-{:02}-{:02} {:02}:{:02}:{:02}", t.year, t.mon, t.day, t.hour, t.min, t.sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_1970() {
        assert_eq!(utc(0), Local { year: 1970, mon: 1, day: 1, hour: 0, min: 0, sec: 0 });
    }

    #[test]
    fn known_dates() {
        // 2000-03-01 12:34:56 UTC = 951914096
        assert_eq!(
            utc(951_914_096_000),
            Local { year: 2000, mon: 3, day: 1, hour: 12, min: 34, sec: 56 }
        );
        // 2024-02-29 00:00:00 UTC = 1709164800
        let t = utc(1_709_164_800_000);
        assert_eq!((t.year, t.mon, t.day), (2024, 2, 29));
    }

    #[test]
    fn same_day_shows_only_hhmm() {
        let now = now_ms();
        let s = format_ts(now, now);
        assert_eq!(s.len(), 5);
        assert_eq!(&s[2..3], ":");
    }
}
