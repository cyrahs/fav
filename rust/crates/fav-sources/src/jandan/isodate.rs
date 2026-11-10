//! `zulu_to_offset`: Jandan's pagination cursor, the last post's UTC time moved to
//! UTC-8 and printed with milliseconds.
//!
//! Python origin: `zulu_to_offset` in `src/web/jandan.py`, which leans on Python 3.12's
//! `datetime.fromisoformat` (ported here from `CPython`'s `_datetimemodule.c`: the
//! `YYYY-MM-DD` and `YYYYMMDD` dates, any single separator character, `HH[:MM[:SS[.f]]]`
//! or `HHMMSS` times, `Z` or `±HH[:MM[:SS[.f]]]` offsets, ISO week dates such as
//! `2026-W07-6`, and the constructor's range errors).

use chrono::{Datelike as _, Duration, NaiveDate, NaiveDateTime, Timelike as _};
use fav_runtime::pystr::repr_str;
use fav_runtime::source::JobError;

/// Jandan's cursor zone, UTC-8.
const OFFSET_HOURS: i64 = -8;

/// One parsed timestamp: the wall time and its offset in microseconds (`None` when naive).
struct Parsed {
    local: NaiveDateTime,
    offset_micros: Option<i64>,
}

fn invalid(text: &str) -> String {
    format!("Invalid isoformat string: {}", repr_str(text))
}

/// The character at `index`, or NUL past the end (C's string terminator).
fn at(chars: &[char], index: usize) -> char {
    chars.get(index).copied().unwrap_or('\0')
}

/// `parse_digits(p, &var, n)`: exactly `n` ASCII digits.
fn digits(chars: &[char], start: usize, count: usize) -> Option<(usize, i64)> {
    let mut value = 0_i64;
    for index in start..start + count {
        let digit = at(chars, index).to_digit(10).filter(|_| at(chars, index).is_ascii_digit())?;
        value = value * 10 + i64::from(digit);
    }
    Some((start + count, value))
}

/// `parse_hh_mm_ss_ff(chars[start..], chars[end])`, ported literally: digits are read
/// from the whole string (C reads on past `end`), and the flag is C's return value 1,
/// "the text did not end here".
fn hh_mm_ss_ff(chars: &[char], start: usize, end: usize) -> Result<([i64; 3], i64, bool), ()> {
    let mut values = [0_i64; 3];
    let mut p = start;
    let mut has_separator = true;
    for (index, slot) in values.iter_mut().enumerate() {
        let (next, value) = digits(chars, p, 2).ok_or(())?;
        *slot = value;
        p = next;
        let c = at(chars, p);
        p += 1;
        if index == 0 {
            has_separator = c == ':';
        }
        if p >= end {
            return Ok((values, 0, c != '\0'));
        }
        if has_separator && c == ':' {
            // The next component follows.
        } else if c == '.' || c == ',' {
            break;
        } else if !has_separator {
            p -= 1;
        } else {
            return Err(());
        }
    }
    let to_parse = end.saturating_sub(p).min(6);
    let (mut next, mut micros) = digits(chars, p, to_parse).ok_or(())?;
    if (1..6).contains(&to_parse) {
        micros *= [100_000, 10_000, 1000, 100, 10][to_parse - 1];
    }
    while at(chars, next).is_ascii_digit() {
        next += 1;
    }
    Ok((values, micros, at(chars, next) != '\0'))
}

/// `_find_isoformat_datetime_separator`: where the date ends; `None` is C's `-1`, which
/// always ends in an invalid string.
fn separator_location(chars: &[char]) -> Option<usize> {
    let len = chars.len();
    if len == 7 {
        return Some(7);
    }
    if at(chars, 4) == '-' {
        if at(chars, 5) != 'W' {
            return Some(10);
        }
        // YYYY-W??
        if len < 8 {
            return None;
        }
        if len > 8 && at(chars, 8) == '-' {
            if len == 9 {
                return None;
            }
            // YYYY-Www-## is read as a hyphen separator at 8.
            return Some(if len > 10 && at(chars, 10).is_ascii_digit() { 8 } else { 10 });
        }
        return Some(8);
    }
    if at(chars, 4) != 'W' {
        return Some(8);
    }
    // YYYYWww (7) or YYYYWwwd (8): count the digits that follow.
    let mut idx = 7;
    while idx < len && at(chars, idx).is_ascii_digit() {
        idx += 1;
    }
    if idx < 9 {
        Some(idx)
    } else if idx % 2 == 0 {
        Some(7)
    } else {
        Some(8)
    }
}

/// `iso_to_ymd`: an ISO week date as year, month and day (the year may leave the
/// constructor's range); `None` is an invalid string.
fn iso_to_ymd(iso_year: i64, iso_week: i64, iso_day: i64) -> Option<(i64, i64, i64)> {
    if !(1..=9999).contains(&iso_year) {
        return None;
    }
    let year = i32::try_from(iso_year).ok()?;
    let weekday = chrono::Weekday::try_from(u8::try_from(iso_day - 1).ok()?).ok()?;
    // chrono refuses week 0, a week 53 the year does not have, and days outside 1..7, as
    // CPython does.
    let date = NaiveDate::from_isoywd_opt(year, u32::try_from(iso_week).ok()?, weekday)?;
    Some((i64::from(date.year()), i64::from(date.month()), i64::from(date.day())))
}

/// `parse_isoformat_date(dtstr, len)`: year, month and day of the date part.
fn parse_date(chars: &[char], len: usize) -> Option<(i64, i64, i64)> {
    let (mut p, year) = digits(chars, 0, 4)?;
    let uses_separator = at(chars, p) == '-';
    if uses_separator {
        p += 1;
    }
    if at(chars, p) == 'W' {
        let (next, iso_week) = digits(chars, p + 1, 2)?;
        p = next;
        let mut iso_day = 1;
        if p < len {
            if uses_separator {
                if at(chars, p) != '-' {
                    return None;
                }
                p += 1;
            }
            iso_day = digits(chars, p, 1)?.1;
        }
        return iso_to_ymd(year, iso_week, iso_day);
    }
    let (mut p, month) = digits(chars, p, 2)?;
    if uses_separator {
        if at(chars, p) != '-' {
            return None;
        }
        p += 1;
    }
    let (_, day) = digits(chars, p, 2)?;
    Some((year, month, day))
}

/// `datetime.fromisoformat(text)`.
fn fromisoformat(text: &str) -> Result<Parsed, String> {
    let chars: Vec<char> = text.chars().collect();
    let separator = separator_location(&chars).ok_or_else(|| invalid(text))?;
    let (year, month, day) = parse_date(&chars, separator).ok_or_else(|| invalid(text))?;

    let mut time = [0_i64; 3];
    let mut micros = 0;
    let mut offset_micros = None;
    if chars.len() > separator {
        let start = separator + 1;
        let end = chars.len();
        let tz = (start..end).find(|&index| matches!(chars[index], 'Z' | '+' | '-')).unwrap_or(end);
        let (values, fraction, trailing) = hh_mm_ss_ff(&chars, start, tz).map_err(|()| invalid(text))?;
        time = values;
        micros = fraction;
        if tz == end {
            if trailing {
                return Err(invalid(text));
            }
        } else if chars[tz] == 'Z' {
            if tz + 1 != end {
                return Err(invalid(text));
            }
            offset_micros = Some(0);
        } else {
            let sign = if chars[tz] == '-' { -1 } else { 1 };
            let ([hours, minutes, seconds], tz_micros, trailing) = hh_mm_ss_ff(&chars, tz + 1, end).map_err(|()| invalid(text))?;
            if trailing {
                return Err(invalid(text));
            }
            let offset = sign * ((hours * 3600 + minutes * 60 + seconds) * 1_000_000 + tz_micros);
            if offset.abs() >= 86_400_000_000 {
                return Err(format!(
                    "offset must be a timedelta strictly between -timedelta(hours=24) and timedelta(hours=24), not {}.",
                    timedelta_repr(offset)
                ));
            }
            offset_micros = Some(offset);
        }
    }
    if !(1..=9999).contains(&year) {
        return Err(format!("year {year} is out of range"));
    }
    if !(1..=12).contains(&month) {
        return Err("month must be in 1..12".to_owned());
    }
    let date = NaiveDate::from_ymd_opt(
        i32::try_from(year).unwrap_or(1),
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(0),
    )
    .ok_or_else(|| "day is out of range for month".to_owned())?;
    let [hour, minute, second] = time;
    if !(0..=23).contains(&hour) {
        return Err("hour must be in 0..23".to_owned());
    }
    if !(0..=59).contains(&minute) {
        return Err("minute must be in 0..59".to_owned());
    }
    if !(0..=59).contains(&second) {
        return Err("second must be in 0..59".to_owned());
    }
    let local = date
        .and_hms_micro_opt(
            u32::try_from(hour).unwrap_or(0),
            u32::try_from(minute).unwrap_or(0),
            u32::try_from(second).unwrap_or(0),
            u32::try_from(micros).unwrap_or(0),
        )
        .ok_or_else(|| invalid(text))?;
    Ok(Parsed { local, offset_micros })
}

/// `repr(timedelta(microseconds=micros))`.
fn timedelta_repr(micros: i64) -> String {
    let days = micros.div_euclid(86_400_000_000);
    let rest = micros.rem_euclid(86_400_000_000);
    let seconds = rest / 1_000_000;
    let microseconds = rest % 1_000_000;
    let mut parts = Vec::new();
    if days != 0 {
        parts.push(format!("days={days}"));
    }
    if seconds != 0 {
        parts.push(format!("seconds={seconds}"));
    }
    if microseconds != 0 {
        parts.push(format!("microseconds={microseconds}"));
    }
    if parts.is_empty() {
        "datetime.timedelta(0)".to_owned()
    } else {
        format!("datetime.timedelta({})", parts.join(", "))
    }
}

/// `zulu_to_offset(date_z)`: a trailing `Z` becomes `+00:00`, the time moves to UTC-8 and
/// is printed as `YYYY-MM-DDTHH:MM:SS.mmm-0800`. A naive time counts as UTC (Python's
/// `astimezone` reads it in the process zone, which is UTC in fav's deployment), with
/// Python's refusal of the first and last day of the calendar on that path.
pub fn zulu_to_offset(date_z: &str) -> Result<String, JobError> {
    let normalized = match date_z.strip_suffix('Z') {
        Some(rest) => format!("{rest}+00:00"),
        None => date_z.to_owned(),
    };
    let parsed = fromisoformat(&normalized).map_err(|message| JobError::new("ValueError", message))?;
    if parsed.offset_micros.is_none() {
        // `astimezone` on a naive time first finds the local zone (`local_to_seconds`),
        // probing a day before and a day after; either probe leaving years 1..9999 is
        // `utc_to_seconds`'s ValueError.
        let date = parsed.local.date();
        if date.pred_opt().is_none_or(|before| before.year() < 1) {
            return Err(JobError::new("ValueError", "year 0 is out of range"));
        }
        if date.succ_opt().is_none_or(|after| after.year() > 9999) {
            return Err(JobError::new("ValueError", "year 10000 is out of range"));
        }
    }
    let utc = parsed.local - Duration::microseconds(parsed.offset_micros.unwrap_or(0));
    let local = utc + Duration::hours(OFFSET_HOURS);
    if !(1..=9999).contains(&local.year()) {
        return Err(JobError::new("OverflowError", "date value out of range"));
    }
    Ok(format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}-0800",
        local.year(),
        local.month(),
        local.day(),
        local.hour(),
        local.minute(),
        local.second(),
        local.nanosecond() / 1_000_000
    ))
}
