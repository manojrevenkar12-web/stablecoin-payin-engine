//! Time without a time library: UTC seconds, civil dates, US Eastern time with the US DST rule,
//! and the USD mint-and-burn window.

pub type Ts = i64; // seconds since 1970-01-01T00:00:00Z

pub const MINUTE: Ts = 60;
pub const HOUR: Ts = 3600;
pub const DAY: Ts = 86_400;

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 0 = Monday … 6 = Sunday
pub fn weekday(days: i64) -> u32 {
    ((days + 3).rem_euclid(7)) as u32
}

pub fn ts(y: i64, m: u32, d: u32, hh: i64, mm: i64, ss: i64) -> Ts {
    days_from_civil(y, m, d) * DAY + hh * HOUR + mm * MINUTE + ss
}

pub fn date_of(t: Ts) -> (i64, u32, u32) {
    civil_from_days(t.div_euclid(DAY))
}

pub fn fmt_date(t: Ts) -> String {
    let (y, m, d) = date_of(t);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn fmt_iso(t: Ts) -> String {
    let (y, m, d) = date_of(t);
    let s = t.rem_euclid(DAY);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", s / HOUR, (s % HOUR) / MINUTE, s % MINUTE)
}

/// Parse `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM:SS(Z|±HH:MM)` (fractional seconds ignored).
pub fn parse_iso(s: &str) -> Option<Ts> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let m: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let base = days_from_civil(y, m, d) * DAY;
    if b.len() == 10 {
        return Some(base);
    }
    if b[10] != b'T' || b.len() < 19 {
        return None;
    }
    let hh: i64 = s.get(11..13)?.parse().ok()?;
    let mm: i64 = s.get(14..16)?.parse().ok()?;
    let ss: i64 = s.get(17..19)?.parse().ok()?;
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        let n = r.bytes().take_while(|c| c.is_ascii_digit()).count();
        rest = &r[n..];
    }
    let offset = match rest {
        "" | "Z" => 0,
        r if (r.starts_with('+') || r.starts_with('-')) && r.len() == 6 => {
            let sign = if r.starts_with('-') { -1 } else { 1 };
            let oh: i64 = r.get(1..3)?.parse().ok()?;
            let om: i64 = r.get(4..6)?.parse().ok()?;
            sign * (oh * HOUR + om * MINUTE)
        }
        _ => return None,
    };
    Some(base + hh * HOUR + mm * MINUTE + ss - offset)
}

/// The n-th (1-based) Sunday of a month, as days since epoch.
fn nth_sunday(y: i64, m: u32, n: u32) -> i64 {
    let first = days_from_civil(y, m, 1);
    let to_sunday = (6 - weekday(first) as i64).rem_euclid(7);
    first + to_sunday + 7 * (n as i64 - 1)
}

/// UTC offset of US Eastern time at instant `t`: −4h in daylight time, −5h otherwise.
/// DST runs from 02:00 local on the second Sunday of March to 02:00 local on the first Sunday of November.
pub fn eastern_offset(t: Ts) -> Ts {
    let (y, _, _) = date_of(t);
    let start = nth_sunday(y, 3, 2) * DAY + 2 * HOUR + 5 * HOUR; // 02:00 EST = 07:00Z
    let end = nth_sunday(y, 11, 1) * DAY + 2 * HOUR + 4 * HOUR; // 02:00 EDT = 06:00Z
    if t >= start && t < end { -4 * HOUR } else { -5 * HOUR }
}

/// Local Eastern wall-clock instant expressed as UTC. Ambiguous/skipped hours resolve to standard time,
/// which does not matter for the 17:00/21:00 boundaries used here.
pub fn eastern_to_utc(y: i64, m: u32, d: u32, hh: i64, mm: i64) -> Ts {
    let naive = ts(y, m, d, hh, mm, 0);
    let guess = naive + 5 * HOUR;
    naive - eastern_offset(guess)
}

/// The USD mint-and-burn window: Monday to Friday, 21:00 ET the previous day to 17:00 ET.
/// Returns true if `t` is inside it.
pub fn in_usd_mint_window(t: Ts, open_h: i64, close_h: i64) -> bool {
    let local = t + eastern_offset(t);
    let days = local.div_euclid(DAY);
    let secs = local.rem_euclid(DAY);
    let wd = weekday(days); // weekday of the local date
    if secs < close_h * HOUR {
        // Morning part of a business day's window.
        wd <= 4
    } else if secs >= open_h * HOUR {
        // Evening opening that belongs to the next business day: Sunday..Thursday evenings.
        wd == 6 || wd <= 3
    } else {
        false
    }
}

/// The next instant at or after `t` that falls inside the window.
pub fn next_usd_window_open(t: Ts, open_h: i64, close_h: i64) -> Ts {
    if in_usd_mint_window(t, open_h, close_h) {
        return t;
    }
    // Walk local days forward and return the first 21:00 ET that opens a window.
    let local = t + eastern_offset(t);
    for day in (local.div_euclid(DAY)..).take(8) {
        let (y, m, d) = civil_from_days(day);
        let open = eastern_to_utc(y, m, d, open_h, 0);
        if open >= t && in_usd_mint_window(open, open_h, close_h) {
            return open;
        }
    }
    unreachable!("a window opens at least once a week")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        for d in [-1000, 0, 1, 365, 20_000, 20_400, 21_000] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
        assert_eq!(days_from_civil(2026, 11, 6), 20_763);
        assert_eq!(weekday(days_from_civil(2026, 11, 6)), 4); // Friday
    }

    #[test]
    fn iso_parse_and_format() {
        let t = ts(2026, 11, 2, 14, 3, 9);
        assert_eq!(parse_iso(&fmt_iso(t)), Some(t));
        assert_eq!(parse_iso("2026-11-02T15:03:09+01:00"), Some(t));
        assert_eq!(parse_iso("2026-11-02T14:03:09.123Z"), Some(t));
        assert_eq!(parse_iso("2026-11-02"), Some(ts(2026, 11, 2, 0, 0, 0)));
        assert_eq!(parse_iso("2026-13-02"), None);
        assert_eq!(parse_iso("garbage"), None);
    }

    #[test]
    fn eastern_time_follows_dst() {
        // 2026: DST from 8 March to 1 November.
        assert_eq!(eastern_offset(ts(2026, 7, 1, 12, 0, 0)), -4 * HOUR);
        assert_eq!(eastern_offset(ts(2026, 12, 1, 12, 0, 0)), -5 * HOUR);
        assert_eq!(eastern_offset(ts(2026, 11, 1, 5, 59, 0)), -4 * HOUR);
        assert_eq!(eastern_offset(ts(2026, 11, 1, 6, 0, 0)), -5 * HOUR);
        assert_eq!(eastern_offset(ts(2026, 3, 8, 6, 59, 0)), -5 * HOUR);
        assert_eq!(eastern_offset(ts(2026, 3, 8, 7, 0, 0)), -4 * HOUR);
    }

    #[test]
    fn mint_window_boundaries() {
        // Tuesday 3 Nov 2026 (EST, UTC-5): 16:59 ET open, 17:00 ET closed, 21:00 ET open again.
        assert!(in_usd_mint_window(eastern_to_utc(2026, 11, 3, 16, 59), 21, 17));
        assert!(!in_usd_mint_window(eastern_to_utc(2026, 11, 3, 17, 0), 21, 17));
        assert!(!in_usd_mint_window(eastern_to_utc(2026, 11, 3, 20, 59), 21, 17));
        assert!(in_usd_mint_window(eastern_to_utc(2026, 11, 3, 21, 0), 21, 17));
        // Friday evening and Saturday are closed; Sunday 21:00 ET opens Monday's window.
        assert!(!in_usd_mint_window(eastern_to_utc(2026, 11, 6, 21, 30), 21, 17));
        assert!(!in_usd_mint_window(eastern_to_utc(2026, 11, 7, 12, 0), 21, 17));
        assert!(in_usd_mint_window(eastern_to_utc(2026, 11, 8, 21, 0), 21, 17));
        let fri_late = eastern_to_utc(2026, 11, 6, 17, 30);
        assert_eq!(next_usd_window_open(fri_late, 21, 17), eastern_to_utc(2026, 11, 8, 21, 0));
        let tue_gap = eastern_to_utc(2026, 11, 3, 18, 0);
        assert_eq!(next_usd_window_open(tue_gap, 21, 17), eastern_to_utc(2026, 11, 3, 21, 0));
    }
}
