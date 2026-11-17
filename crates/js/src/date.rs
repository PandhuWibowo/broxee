//! `Date` arithmetic, parsing and formatting, without the interpreter: times are milliseconds
//! since 1970 UTC as `f64` (`NaN` for an invalid date), as ECMAScript keeps them.
//!
//! Local time is UTC moved by an offset the embedder supplies (`Offset`): the system's time
//! zone in the browser, none in replays, so a saved page renders the same everywhere.
//! Calendar conversion is Howard Hinnant's `days_from_civil` / `civil_from_days`: exact for
//! every year a `Date` can hold, leap years included.

/// Milliseconds to add to a UTC time to get local time, for that UTC time.
pub type Offset = fn(f64) -> f64;

/// UTC everywhere: no daylight saving, no zone.
pub fn utc(_: f64) -> f64 {
    0.0
}

const MS_PER_DAY: f64 = 86_400_000.0;
/// ±100,000,000 days around 1970: the range a `Date` holds (ECMA-262 TimeClip).
const MAX_TIME: f64 = 8.64e15;

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Days since 1970-01-01 of a proleptic Gregorian date (`month` 1 to 12).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `days` after 1970-01-01: (year, month 1 to 12, day 1 to 31).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// The fields of a time, as `Date`'s getters give them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fields {
    pub year: f64,
    /// 0 to 11.
    pub month: f64,
    /// 1 to 31.
    pub day: f64,
    pub hours: f64,
    pub minutes: f64,
    pub seconds: f64,
    pub ms: f64,
    /// 0 (Sunday) to 6.
    pub weekday: f64,
}

impl Fields {
    /// As an array in setter order: year, month, day, hours, minutes, seconds, ms.
    pub fn to_array(self) -> [f64; 7] {
        [
            self.year,
            self.month,
            self.day,
            self.hours,
            self.minutes,
            self.seconds,
            self.ms,
        ]
    }
}

/// The fields of finite time `t` (already moved to local time if wanted).
pub fn fields(t: f64) -> Fields {
    let days = (t / MS_PER_DAY).floor();
    let ms_in_day = t - days * MS_PER_DAY;
    let (year, month, day) = civil_from_days(days as i64);
    Fields {
        year: year as f64,
        month: (month - 1) as f64,
        day: day as f64,
        hours: (ms_in_day / 3_600_000.0).floor(),
        minutes: (ms_in_day / 60_000.0).floor() % 60.0,
        seconds: (ms_in_day / 1000.0).floor() % 60.0,
        ms: ms_in_day % 1000.0,
        weekday: (days + 4.0).rem_euclid(7.0),
    }
}

/// ECMA-262 MakeDay + MakeTime + MakeDate from fields (month may be out of 0..11, any field
/// may overflow into the next): milliseconds, or `NaN` when a field is not finite.
pub fn make(f: [f64; 7]) -> f64 {
    if f.iter().any(|v| !v.is_finite()) {
        return f64::NAN;
    }
    let [year, month, day, h, m, s, ms] = f.map(f64::trunc);
    let y = year + (month / 12.0).floor();
    let mn = month.rem_euclid(12.0);
    if y.abs() > 400_000.0 {
        return f64::NAN;
    }
    let days = days_from_civil(y as i64, mn as i64 + 1, 1) as f64 + day - 1.0;
    let time = h * 3_600_000.0 + m * 60_000.0 + s * 1000.0 + ms;
    days * MS_PER_DAY + time
}

/// ECMA-262 TimeClip: whole milliseconds within the range, else `NaN`.
pub fn clip(t: f64) -> f64 {
    if !t.is_finite() || t.abs() > MAX_TIME {
        f64::NAN
    } else {
        t.trunc() + 0.0
    }
}

/// UTC time of local time `local`: the offset is looked up at the UTC time it gives, so
/// times next to a daylight-saving change land on the right side.
pub fn local_to_utc(local: f64, offset: Offset) -> f64 {
    if !local.is_finite() {
        return f64::NAN;
    }
    let guess = local - offset(local);
    local - offset(guess)
}

fn sign_offset(offset_ms: f64) -> String {
    let minutes = (offset_ms / 60_000.0).round() as i64;
    let sign = if minutes < 0 { '-' } else { '+' };
    let m = minutes.abs();
    format!("{sign}{:02}{:02}", m / 60, m % 60)
}

fn year_text(year: f64) -> String {
    if year < 0.0 {
        format!("-{:06}", -year)
    } else {
        format!("{year:04}")
    }
}

/// `toISOString`: `2026-11-17T13:54:00.000Z`, `None` for an invalid date (a RangeError).
pub fn to_iso(t: f64) -> Option<String> {
    if t.is_nan() {
        return None;
    }
    let f = fields(t);
    let year = if (0.0..=9999.0).contains(&f.year) {
        format!("{:04}", f.year)
    } else if f.year < 0.0 {
        format!("-{:06}", -f.year)
    } else {
        format!("+{:06}", f.year)
    };
    Some(format!(
        "{year}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        f.month + 1.0,
        f.day,
        f.hours,
        f.minutes,
        f.seconds,
        f.ms
    ))
}

/// `toDateString`: `Tue Nov 17 2026`, of local time `local`.
pub fn date_text(local: f64) -> String {
    let f = fields(local);
    format!(
        "{} {} {:02} {}",
        DAYS[f.weekday as usize],
        MONTHS[f.month as usize],
        f.day,
        year_text(f.year)
    )
}

/// `toTimeString`: `20:54:00 GMT+0700`.
pub fn time_text(local: f64, offset_ms: f64) -> String {
    let f = fields(local);
    format!(
        "{:02}:{:02}:{:02} GMT{}",
        f.hours,
        f.minutes,
        f.seconds,
        sign_offset(offset_ms)
    )
}

/// `toString`: `Tue Nov 17 2026 20:54:00 GMT+0700`, or `Invalid Date`.
pub fn to_text(t: f64, offset: Offset) -> String {
    if t.is_nan() {
        return "Invalid Date".into();
    }
    let off = offset(t);
    format!("{} {}", date_text(t + off), time_text(t + off, off))
}

/// `toUTCString` (and `toGMTString`): `Tue, 17 Nov 2026 13:54:00 GMT`.
pub fn to_utc_text(t: f64) -> String {
    if t.is_nan() {
        return "Invalid Date".into();
    }
    let f = fields(t);
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[f.weekday as usize],
        f.day,
        MONTHS[f.month as usize],
        year_text(f.year),
        f.hours,
        f.minutes,
        f.seconds
    )
}

/// `toLocaleDateString` for `en-US`: `11/17/2026`.
pub fn locale_date(local: f64) -> String {
    let f = fields(local);
    format!("{}/{}/{}", f.month + 1.0, f.day, f.year)
}

/// `toLocaleTimeString` for `en-US`: `8:54:00 PM`.
pub fn locale_time(local: f64) -> String {
    let f = fields(local);
    let h12 = match f.hours % 12.0 {
        0.0 => 12.0,
        h => h,
    };
    let half = if f.hours < 12.0 { "AM" } else { "PM" };
    format!("{h12}:{:02}:{:02} {half}", f.minutes, f.seconds)
}

/// `Date.parse`: ISO 8601 as ECMAScript defines it (a date alone is UTC, a date and time
/// without a zone is local), then the forms `toString` and `toUTCString` write and the
/// common ones browsers accept (`Nov 17, 2026`, `2026/11/17 10:00`, `11/17/2026`).
pub fn parse(text: &str, offset: Offset) -> f64 {
    let text = text.trim();
    match parse_iso(text) {
        Some((local, Some(zone))) => clip(local - zone),
        Some((local, None)) => clip(local_to_utc(local, offset)),
        None => match parse_loose(text) {
            Some((local, Some(zone))) => clip(local - zone),
            Some((local, None)) => clip(local_to_utc(local, offset)),
            None => f64::NAN,
        },
    }
}

/// A run of `n` ASCII digits at the start of `s`, as a number, and the rest.
fn digits(s: &str, n: usize) -> Option<(f64, &str)> {
    let head = s.get(..n)?;
    head.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| (head.parse().unwrap_or(0.0), &s[n..]))
}

/// ISO: the time (as if UTC) and the zone offset in ms when one was written; date-only
/// forms count as written in UTC.
fn parse_iso(s: &str) -> Option<(f64, Option<f64>)> {
    let (year, mut rest) = match s.as_bytes().first()? {
        b'+' | b'-' => {
            let (y, r) = digits(&s[1..], 6)?;
            if s.starts_with('-') && y == 0.0 {
                return None; // -000000 is not a year
            }
            (if s.starts_with('-') { -y } else { y }, r)
        }
        _ => digits(s, 4)?,
    };
    let mut month = 1.0;
    let mut day = 1.0;
    if let Some(r) = rest.strip_prefix('-') {
        let (m, r) = digits(r, 2)?;
        month = m;
        rest = r;
        if let Some(r) = rest.strip_prefix('-') {
            let (d, r) = digits(r, 2)?;
            day = d;
            rest = r;
        }
    }
    if !(1.0..=12.0).contains(&month) || !(1.0..=31.0).contains(&day) {
        return None;
    }
    if rest.is_empty() {
        return Some((
            make([year, month - 1.0, day, 0.0, 0.0, 0.0, 0.0]),
            Some(0.0),
        ));
    }
    // Browsers take a space for the `T`.
    let rest = rest.strip_prefix('T').or_else(|| rest.strip_prefix(' '))?;
    let (h, r) = digits(rest, 2)?;
    let r = r.strip_prefix(':')?;
    let (m, mut r) = digits(r, 2)?;
    let (mut sec, mut ms) = (0.0, 0.0);
    if let Some(rr) = r.strip_prefix(':') {
        let (sv, rr) = digits(rr, 2)?;
        sec = sv;
        r = rr;
        if let Some(rr) = r.strip_prefix('.').or_else(|| r.strip_prefix(',')) {
            let n = rr.bytes().take_while(u8::is_ascii_digit).count();
            if n == 0 {
                return None;
            }
            // Milliseconds from the first three digits, the rest dropped.
            let frac = &rr[..n];
            let three = format!("{frac:0<3}");
            ms = three[..3].parse().unwrap_or(0.0);
            r = &rr[n..];
        }
    }
    if h > 24.0 || m > 59.0 || sec > 59.0 || (h == 24.0 && (m, sec, ms) != (0.0, 0.0, 0.0)) {
        return None;
    }
    let zone = match r {
        "" => None,
        "Z" | "z" => Some(0.0),
        _ => {
            let sign = match r.as_bytes()[0] {
                b'+' => 1.0,
                b'-' => -1.0,
                _ => return None,
            };
            let (zh, zr) = digits(&r[1..], 2)?;
            let zr = zr.strip_prefix(':').unwrap_or(zr);
            let (zm, zr) = digits(zr, 2)?;
            if !zr.is_empty() || zh > 23.0 || zm > 59.0 {
                return None;
            }
            Some(sign * (zh * 3_600_000.0 + zm * 60_000.0))
        }
    };
    Some((make([year, month - 1.0, day, h, m, sec, ms]), zone))
}

/// The legacy forms, read as tokens: weekday names are skipped, a month name sets the month,
/// `hh:mm[:ss]` the time, `AM`/`PM` adjust it, `GMT`/`UTC`/`Z` with an optional `+hhmm` the
/// zone, and the remaining numbers are the day and year (`a/b/c` is month/day/year, or
/// year/month/day when it starts with four digits).
fn parse_loose(s: &str) -> Option<(f64, Option<f64>)> {
    let mut year: Option<f64> = None;
    let mut month: Option<f64> = None;
    let mut day: Option<f64> = None;
    let mut time = [0.0; 4];
    let mut half: Option<bool> = None;
    let mut zone: Option<f64> = None;
    let mut numbers: Vec<f64> = Vec::new();
    // `01-Jan-70` (cookie dates): a dashed token with a month name is its parts.
    let tokens: Vec<&str> = s
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .flat_map(|t| {
            let named = t.contains('-')
                && !t.starts_with(['-', '+'])
                && t.bytes().any(|b| b.is_ascii_alphabetic())
                && !t.to_ascii_lowercase().starts_with("gmt")
                && !t.to_ascii_lowercase().starts_with("utc");
            let parts: Vec<&str> = if named {
                t.split('-').filter(|p| !p.is_empty()).collect()
            } else {
                vec![t]
            };
            parts
        })
        .collect();
    for token in tokens {
        // "(Western Indonesia Time)" after toString: a comment.
        if token.starts_with('(') {
            break;
        }
        let lower = token.to_ascii_lowercase();
        let word = lower.trim_end_matches('.');
        if let Some(i) = MONTHS
            .iter()
            .position(|m| word.len() >= 3 && m.to_ascii_lowercase().starts_with(&word[..3]))
            .filter(|_| word.bytes().all(|b| b.is_ascii_alphabetic()))
        {
            month = Some(i as f64);
            continue;
        }
        if DAYS
            .iter()
            .any(|d| word.len() >= 3 && word.starts_with(&d.to_ascii_lowercase()))
        {
            continue;
        }
        match word {
            "am" | "pm" => {
                half = Some(word == "pm");
                continue;
            }
            "gmt" | "utc" | "ut" | "z" => {
                zone = Some(0.0);
                continue;
            }
            _ => {}
        }
        // GMT+0700, UTC-05:00, or a bare +0700 after the time.
        let after_zone = lower
            .strip_prefix("gmt")
            .or_else(|| lower.strip_prefix("utc"))
            .unwrap_or(&lower);
        if after_zone.starts_with(['+', '-']) && (zone.is_some() || after_zone.len() > 3) {
            let sign = if after_zone.starts_with('-') {
                -1.0
            } else {
                1.0
            };
            let body: String = after_zone[1..].chars().filter(|c| *c != ':').collect();
            let (zh, zm) = match body.len() {
                1 | 2 => (body.parse::<f64>().ok()?, 0.0),
                4 => (body[..2].parse().ok()?, body[2..].parse().ok()?),
                _ => return None,
            };
            zone = Some(sign * (zh * 3_600_000.0 + zm * 60_000.0));
            continue;
        }
        if token.contains(':') {
            let parts: Vec<&str> = token.split(':').collect();
            if parts.len() > 3 {
                return None;
            }
            for (i, p) in parts.iter().enumerate() {
                let (whole, frac) = p.split_once('.').unwrap_or((p, ""));
                time[i] = whole.parse().ok()?;
                if i == 2 && !frac.is_empty() {
                    time[3] = format!("{frac:0<3}")[..3].parse().ok()?;
                }
            }
            continue;
        }
        if token.contains('/') || (token.contains('-') && !token.starts_with('-')) {
            let parts: Vec<f64> = token
                .split(['/', '-'])
                .map(|p| p.parse().ok())
                .collect::<Option<_>>()?;
            match parts[..] {
                [y, m, d] if token.split(['/', '-']).next()?.len() == 4 => {
                    (year, month, day) = (Some(y), Some(m - 1.0), Some(d));
                }
                [m, d, y] => (year, month, day) = (Some(y), Some(m - 1.0), Some(d)),
                [m, d] => (month, day) = (Some(m - 1.0), Some(d)),
                _ => return None,
            }
            continue;
        }
        numbers.push(token.parse().ok()?);
    }
    for n in numbers {
        if day.is_none() && (1.0..=31.0).contains(&n) && month.is_some() {
            day = Some(n);
        } else if year.is_none() {
            year = Some(n);
        } else if day.is_none() {
            day = Some(n);
        } else {
            return None;
        }
    }
    let mut year = year?;
    if (0.0..50.0).contains(&year) {
        year += 2000.0;
    } else if (50.0..100.0).contains(&year) {
        year += 1900.0;
    }
    let (month, day) = (month?, day.unwrap_or(1.0));
    if !(0.0..12.0).contains(&month) || !(1.0..=31.0).contains(&day) {
        return None;
    }
    let mut hours = time[0];
    match half {
        Some(pm) if (1.0..=12.0).contains(&hours) => {
            hours = hours % 12.0 + if pm { 12.0 } else { 0.0 }
        }
        Some(_) => return None,
        None => {}
    }
    Some((
        make([year, month, day, hours, time[1], time[2], time[3]]),
        zone,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UTC+7, no daylight saving (Jakarta).
    fn jakarta(_: f64) -> f64 {
        7.0 * 3_600_000.0
    }

    const T: f64 = 1_794_923_640_000.0; // 2026-11-17T13:54:00Z, a Tuesday

    #[test]
    fn calendar_round_trips_every_day_of_four_centuries() {
        for days in -146_097 * 2..146_097 {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(days_from_civil(2000, 2, 29), 11_016);
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn fields_and_make_agree() {
        let f = fields(T);
        assert_eq!(
            f.to_array(),
            [2026.0, 10.0, 17.0, 13.0, 54.0, 0.0, 0.0],
            "{f:?}"
        );
        assert_eq!(f.weekday, 2.0);
        assert_eq!(make(f.to_array()), T);
        // Overflow carries: the 32nd of December is 1 January.
        assert_eq!(
            make([2026.0, 11.0, 32.0, 0.0, 0.0, 0.0, 0.0]),
            make([2027.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0])
        );
        // Month -1 is December of the year before.
        assert_eq!(
            fields(make([2026.0, -1.0, 1.0, 0.0, 0.0, 0.0, 0.0])).year,
            2025.0
        );
        let before = fields(-1.0);
        assert_eq!(
            (before.year, before.ms, before.weekday),
            (1969.0, 999.0, 3.0)
        );
        assert!(make([f64::NAN, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]).is_nan());
    }

    #[test]
    fn writes_every_form() {
        assert_eq!(to_iso(T).as_deref(), Some("2026-11-17T13:54:00.000Z"));
        assert_eq!(to_iso(f64::NAN), None);
        assert_eq!(
            to_iso(make([-1.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0])).as_deref(),
            Some("-000001-01-01T00:00:00.000Z")
        );
        assert_eq!(to_utc_text(T), "Tue, 17 Nov 2026 13:54:00 GMT");
        assert_eq!(to_text(T, jakarta), "Tue Nov 17 2026 20:54:00 GMT+0700");
        assert_eq!(to_text(T, utc), "Tue Nov 17 2026 13:54:00 GMT+0000");
        assert_eq!(to_text(f64::NAN, utc), "Invalid Date");
        assert_eq!(locale_date(T + jakarta(T)), "11/17/2026");
        assert_eq!(locale_time(T + jakarta(T)), "8:54:00 PM");
        assert_eq!(
            locale_time(make([2026.0, 0.0, 1.0, 0.0, 5.0, 9.0, 0.0])),
            "12:05:09 AM"
        );
    }

    #[test]
    fn parses_iso_with_its_zone_rules() {
        assert_eq!(parse("2026-11-17T13:54:00Z", jakarta), T);
        assert_eq!(parse("2026-11-17T13:54:00.000+00:00", jakarta), T);
        assert_eq!(parse("2026-11-17T20:54+0700", utc), T);
        // A date alone is UTC; a date and time without a zone is local.
        assert_eq!(
            parse("2026-11-17", jakarta),
            T - 13.0 * 3_600_000.0 - 54.0 * 60_000.0
        );
        assert_eq!(parse("2026-11-17T20:54:00", jakarta), T);
        assert_eq!(
            parse("2026-11-17 20:54:00", jakarta),
            T,
            "a space for the T"
        );
        assert_eq!(
            parse("2026-11", utc),
            make([2026.0, 10.0, 1.0, 0.0, 0.0, 0.0, 0.0])
        );
        assert_eq!(
            parse("2026", utc),
            make([2026.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0])
        );
        assert_eq!(parse("2026-11-17T13:54:00.1234Z", utc), T + 123.0);
        assert_eq!(parse("+002026-11-17T13:54:00Z", utc), T);
        for bad in [
            "2026-13-01",
            "2026-11-17T25:00Z",
            "2026-11-17T13:54:00+7",
            "nope",
            "",
        ] {
            assert!(parse(bad, utc).is_nan(), "{bad}");
        }
    }

    #[test]
    fn parses_what_browsers_write_and_accept() {
        assert_eq!(parse("Tue, 17 Nov 2026 13:54:00 GMT", jakarta), T);
        assert_eq!(parse(&to_text(T, jakarta), utc), T, "toString reads back");
        assert_eq!(
            parse(
                "Tue Nov 17 2026 20:54:00 GMT+0700 (Western Indonesia Time)",
                utc
            ),
            T
        );
        assert_eq!(parse("Nov 17, 2026 20:54:00", jakarta), T);
        assert_eq!(parse("November 17 2026 8:54 PM", jakarta), T);
        assert_eq!(parse("17 Nov 2026 13:54:00 UTC", jakarta), T);
        assert_eq!(parse("2026/11/17 20:54", jakarta), T);
        assert_eq!(parse("11/17/2026 20:54:00", jakarta), T);
        assert_eq!(
            parse("Thursday, 01-Jan-70 00:00:00 GMT", utc),
            0.0,
            "cookie dates"
        );
        for bad in ["Nov 2026 13:99:00 PM x", "hello world", "13:54"] {
            assert!(parse(bad, utc).is_nan(), "{bad}");
        }
    }

    #[test]
    fn clips_to_the_range() {
        assert_eq!(clip(8.64e15), 8.64e15);
        assert!(clip(8.64e15 + 1.0).is_nan());
        assert_eq!(clip(1.9), 1.0);
        assert!(clip(f64::INFINITY).is_nan());
    }
}
