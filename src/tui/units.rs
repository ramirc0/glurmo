//! Numbers and times written short, for narrow columns.

use jiff::tz::TimeZone;

use crate::model::UnixTime;

/// Writes `1205` as `1,205`.
pub fn thousands(number: u64) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Writes a duration in its largest whole unit: `45s`, `12m`, `5h`, `3d`.
pub fn span(seconds: i64) -> String {
    let seconds = seconds.max(0);
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// How long ago `then` was at `now`.
pub fn since(then: UnixTime, now: UnixTime) -> String {
    span(now.0 - then.0)
}

/// Writes `at` in `tz` with a `strftime` format.
pub fn clock(at: UnixTime, tz: &TimeZone, format: &str) -> String {
    jiff::Timestamp::from_second(at.0).map_or_else(
        |_| String::new(),
        |time| time.to_zoned(tz.clone()).strftime(format).to_string(),
    )
}

/// Writes a load average in five characters or fewer: `0.06`, `48.6`, `378`.
pub fn load(load: f64) -> String {
    if load < 10.0 {
        format!("{load:.2}")
    } else if load < 100.0 {
        format!("{load:.1}")
    } else {
        format!("{load:.0}")
    }
}

/// `part` as a whole percentage of `whole`, or 0 when `whole` is 0.
pub fn percent(part: u64, whole: u64) -> u64 {
    (part * 100).checked_div(whole).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_spans_read_short() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_205), "1,205");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(span(-5), "0s");
        assert_eq!(span(59), "59s");
        assert_eq!(span(3_599), "59m");
        assert_eq!(span(90_000), "1d");
        assert_eq!(load(0.061), "0.06");
        assert_eq!(load(48.62), "48.6");
        assert_eq!(load(377.6), "378");
        assert_eq!(percent(1, 3), 33);
        assert_eq!(percent(1, 0), 0);
        assert_eq!(clock(UnixTime(86_399), &TimeZone::UTC, "%H:%M"), "23:59");
    }
}
