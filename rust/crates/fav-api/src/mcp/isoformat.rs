//! `datetime.fromisoformat` as `CPython` 3.12 implements it.
//!
//! Python origin: `Modules/_datetimemodule.c` (`datetime_fromisoformat` and the parsers it
//! calls), which is what runs in production, not `_pydatetime.py`: the C parser is more
//! lenient in places (`17.5` is half past five seconds past 17:00, a character before the
//! offset is skipped). The parser reads the UTF-8 bytes of the string as C does, with a
//! NUL after the last byte, so reads past the end behave as they do there.
//! `rust/goldens/mcp/ops.json` (`parse_since`) pins the behaviour against Python.

use chrono::{DateTime, Datelike as _, FixedOffset, NaiveDate, NaiveTime, TimeDelta, TimeZone as _};

/// The parsed value: aware, or naive (no offset in the string).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Parsed {
    Aware(DateTime<FixedOffset>),
    Naive(chrono::NaiveDateTime),
}

/// The string as C sees it: its bytes and a terminating NUL.
struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn at(&self, index: usize) -> u8 {
        self.0.get(index).copied().unwrap_or(0)
    }
}

fn is_digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

/// `parse_digits`: `count` ASCII digits from `pos`, or `None`.
fn parse_digits(text: &Bytes<'_>, pos: &mut usize, value: &mut i64, count: usize) -> bool {
    for _ in 0..count {
        let byte = text.at(*pos);
        *pos += 1;
        if !is_digit(byte) {
            return false;
        }
        *value = *value * 10 + i64::from(byte - b'0');
    }
    true
}

/// `iso_to_ymd`.
fn iso_to_ymd(year: i64, week: i64, day: i64) -> Option<(i64, i64, i64)> {
    if !(1..=9999).contains(&year) {
        return None;
    }
    let year32 = i32::try_from(year).ok()?;
    if !(1..53).contains(&week) {
        let mut out_of_range = true;
        if week == 53 {
            // Years starting on a Thursday, and leap years starting on a Wednesday.
            let first = NaiveDate::from_ymd_opt(year32, 1, 1)?.weekday().num_days_from_monday();
            let leap = NaiveDate::from_ymd_opt(year32, 2, 29).is_some();
            if first == 3 || (first == 2 && leap) {
                out_of_range = false;
            }
        }
        if out_of_range {
            return None;
        }
    }
    if !(1..8).contains(&day) {
        return None;
    }
    // Monday of week 1: the week holding January 4th.
    let jan4 = NaiveDate::from_ymd_opt(year32, 1, 4)?;
    let monday = jan4 - TimeDelta::days(i64::from(jan4.weekday().num_days_from_monday()));
    let date = monday.checked_add_signed(TimeDelta::days((week - 1) * 7 + day - 1))?;
    Some((i64::from(date.year()), i64::from(date.month()), i64::from(date.day())))
}

/// `parse_isoformat_date` over `len` bytes: `Some((y, m, d))` or `None`. The month and
/// day are not range-checked here (the constructor does that).
fn parse_date(text: &Bytes<'_>, len: usize) -> Option<(i64, i64, i64)> {
    let mut pos = 0;
    let mut year = 0;
    if !parse_digits(text, &mut pos, &mut year, 4) {
        return None;
    }
    let uses_separator = text.at(pos) == b'-';
    if uses_separator {
        pos += 1;
    }
    if text.at(pos) == b'W' {
        pos += 1;
        let (mut week, mut day) = (0, 0);
        if !parse_digits(text, &mut pos, &mut week, 2) {
            return None;
        }
        if pos < len {
            if uses_separator {
                let separator = text.at(pos);
                pos += 1;
                if separator != b'-' {
                    return None;
                }
            }
            if !parse_digits(text, &mut pos, &mut day, 1) {
                return None;
            }
        } else {
            day = 1;
        }
        return iso_to_ymd(year, week, day);
    }
    let (mut month, mut day) = (0, 0);
    if !parse_digits(text, &mut pos, &mut month, 2) {
        return None;
    }
    if uses_separator {
        let separator = text.at(pos);
        pos += 1;
        if separator != b'-' {
            return None;
        }
    }
    if !parse_digits(text, &mut pos, &mut day, 2) {
        return None;
    }
    Some((year, month, day))
}

/// `parse_hh_mm_ss_ff` from `start` up to `end`: the four components and whether
/// anything is left before the NUL (`Ok(true)`), or a failure.
fn parse_time_components(text: &Bytes<'_>, start: usize, end: usize) -> Result<([i64; 4], bool), ()> {
    const CORRECTION: [i64; 5] = [100_000, 10_000, 1000, 100, 10];
    let mut values = [0_i64; 4];
    let mut pos = start;
    let mut has_separator = true;
    for (index, slot) in values.iter_mut().take(3).enumerate() {
        if !parse_digits(text, &mut pos, slot, 2) {
            return Err(());
        }
        let ch = text.at(pos);
        pos += 1;
        if index == 0 {
            has_separator = ch == b':';
        }
        if pos >= end {
            return Ok((values, ch != 0));
        }
        if ch == b'.' || ch == b',' {
            break;
        }
        if !has_separator {
            pos -= 1;
        } else if ch != b':' {
            return Err(());
        }
    }
    // After three components without a fraction mark, C goes on to parse the fraction
    // from wherever it stands; so does this.
    let remaining = end.saturating_sub(pos);
    let to_parse = remaining.min(6);
    let mut micros = 0;
    if !parse_digits(text, &mut pos, &mut micros, to_parse) {
        return Err(());
    }
    if to_parse < 6 {
        // `correction[to_parse - 1]`: with nothing to parse C reads before the table,
        // which cannot happen here (a fraction mark is followed by at least the NUL).
        micros *= CORRECTION.get(to_parse.wrapping_sub(1)).copied().unwrap_or(1);
    }
    values[3] = micros;
    while is_digit(text.at(pos)) {
        pos += 1;
    }
    Ok((values, text.at(pos) != 0))
}

/// What `parse_isoformat_time` found: the time and an offset in seconds and microseconds.
struct TimeParts {
    time: [i64; 4],
    offset: Option<(i64, i64)>,
}

/// `parse_isoformat_time` over the bytes from `start` to the end.
fn parse_time(text: &Bytes<'_>, start: usize) -> Option<TimeParts> {
    let end = text.0.len();
    let mut tz_pos = start;
    loop {
        let byte = text.at(tz_pos);
        if byte == b'Z' || byte == b'+' || byte == b'-' {
            break;
        }
        tz_pos += 1;
        if tz_pos >= end {
            break;
        }
    }
    let (time, rest) = parse_time_components(text, start, tz_pos).ok()?;
    if tz_pos == end {
        return if rest { None } else { Some(TimeParts { time, offset: None }) };
    }
    if text.at(tz_pos) == b'Z' {
        return if text.at(tz_pos + 1) != 0 {
            None
        } else {
            Some(TimeParts {
                time,
                offset: Some((0, 0)),
            })
        };
    }
    let sign = if text.at(tz_pos) == b'-' { -1 } else { 1 };
    let (tz, rest) = parse_time_components(text, tz_pos + 1, end).ok()?;
    if rest {
        return None;
    }
    Some(TimeParts {
        time,
        offset: Some((sign * (tz[0] * 3600 + tz[1] * 60 + tz[2]), sign * tz[3])),
    })
}

/// `_find_isoformat_datetime_separator`.
fn find_separator(text: &Bytes<'_>, len: usize) -> Option<usize> {
    if len == 7 {
        return Some(7);
    }
    if text.at(4) == b'-' {
        if text.at(5) == b'W' {
            if len < 8 {
                return None;
            }
            if len > 8 && text.at(8) == b'-' {
                if len == 9 {
                    return None;
                }
                if len > 10 && is_digit(text.at(10)) {
                    return Some(8);
                }
                return Some(10);
            }
            return Some(8);
        }
        return Some(10);
    }
    if text.at(4) == b'W' {
        let mut index = 7;
        while index < len && is_digit(text.at(index)) {
            index += 1;
        }
        if index < 9 {
            return Some(index);
        }
        return Some(if index % 2 == 0 { 7 } else { 8 });
    }
    Some(8)
}

/// The UTF-8 length of the character starting with `byte`, as C counts it.
fn char_width(byte: u8) -> usize {
    if byte & 0x80 == 0 {
        1
    } else {
        match byte & 0xf0 {
            0xe0 => 3,
            0xf0 => 4,
            _ => 2,
        }
    }
}

/// `datetime.fromisoformat(text)`, or `None` for any `ValueError`.
pub(crate) fn fromisoformat(text: &str) -> Option<Parsed> {
    // `_sanitize_isoformat_str`: shorter than 7 characters is invalid. (Its surrogate
    // replacement has no counterpart: a Rust string holds no surrogates.)
    if text.chars().count() < 7 {
        return None;
    }
    let bytes = Bytes(text.as_bytes());
    let len = text.len();
    let separator = find_separator(&bytes, len)?;
    let (year, month, day) = parse_date(&bytes, separator)?;
    let mut parts = TimeParts {
        time: [0; 4],
        offset: None,
    };
    if len > separator {
        let start = separator + char_width(bytes.at(separator));
        if start > len {
            return None;
        }
        // `len -= (p - dt_ptr)`: the time is read from `start` to the end.
        parts = parse_time(&bytes, start)?;
    }
    build(year, month, day, &parts)
}

/// The constructor's range checks: `datetime(y, m, d, H, M, S, us, tzinfo)`.
fn build(year: i64, month: i64, day: i64, parts: &TimeParts) -> Option<Parsed> {
    let [hour, minute, second, micros] = parts.time;
    let date = NaiveDate::from_ymd_opt(
        i32::try_from(year).ok().filter(|year| (1..=9999).contains(year))?,
        u32::try_from(month).ok()?,
        u32::try_from(day).ok()?,
    )?;
    if hour > 23 || minute > 59 || second > 59 || micros > 999_999 {
        return None;
    }
    let time = NaiveTime::from_hms_micro_opt(
        u32::try_from(hour).ok()?,
        u32::try_from(minute).ok()?,
        u32::try_from(second).ok()?,
        u32::try_from(micros).ok()?,
    )?;
    let naive = date.and_time(time);
    let Some((seconds, micros)) = parts.offset else {
        return Some(Parsed::Naive(naive));
    };
    // `timezone(timedelta(seconds=..., microseconds=...))` must be strictly within a day.
    // chrono offsets are whole seconds; a sub-second offset (`+05:30:15.5`) is kept by
    // shifting the instant, which is all `parse_since` uses.
    let total_micros = seconds * 1_000_000 + micros;
    if total_micros.abs() >= 86_400 * 1_000_000 {
        return None;
    }
    let whole = total_micros.div_euclid(1_000_000);
    let fraction = total_micros.rem_euclid(1_000_000);
    let offset = FixedOffset::east_opt(i32::try_from(whole).ok()?)?;
    let local = offset.from_local_datetime(&naive).single()?;
    Some(Parsed::Aware(local - TimeDelta::microseconds(fraction)))
}

#[cfg(test)]
mod tests {
    use super::{Parsed, fromisoformat};

    fn iso(text: &str) -> Option<String> {
        fromisoformat(text).map(|parsed| match parsed {
            Parsed::Aware(value) => value.to_rfc3339(),
            Parsed::Naive(value) => value.to_string(),
        })
    }

    #[test]
    fn the_c_parser_shapes() {
        assert_eq!(iso("2026-10-12T17:00:00Z").as_deref(), Some("2026-10-12T17:00:00+00:00"));
        assert_eq!(iso("2026-10-12").as_deref(), Some("2026-10-12 00:00:00"));
        assert_eq!(iso("2026W411").as_deref(), Some("2026-10-05 00:00:00"));
        assert_eq!(iso("2026-10-12T17.5").as_deref(), Some("2026-10-12 17:00:00.500"));
        assert_eq!(iso("2026-10-12T17:00:00 +05:00").as_deref(), Some("2026-10-12T17:00:00+05:00"));
        assert_eq!(iso("2026-10-12 24:00"), None);
        assert_eq!(iso("2026-10-1"), None);
    }
}
