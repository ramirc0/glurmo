//! Numbers and durations written short, for narrow columns.

use crate::UnixTime;

/// Megabytes as whole gigabytes with a `G`, terabytes with one decimal and a `T`, or, under a
/// gigabyte, megabytes with an `M`.
///
/// # Examples
///
/// ```
/// use glurmo_model::units::gigabytes;
///
/// assert_eq!(gigabytes(512), "512M");
/// assert_eq!(gigabytes(64_000), "62G");
/// assert_eq!(gigabytes(2_000_000), "1.9T");
/// ```
pub fn gigabytes(mb: u64) -> String {
    if mb >= 1_000_000 {
        #[expect(clippy::cast_precision_loss, reason = "megabytes fit in an f64")]
        let tb = mb as f64 / 1_048_576.0;
        format!("{tb:.1}T")
    } else if mb < 1_024 {
        format!("{mb}M")
    } else {
        format!("{}G", mb / 1_024)
    }
}

/// Writes a count as [`thousands`] does, or `–` for none.
///
/// # Examples
///
/// ```
/// use glurmo_model::units::count;
///
/// assert_eq!((count(0), count(1_024)), ("–".to_owned(), "1,024".to_owned()));
/// ```
pub fn count(number: u64) -> String {
    if number == 0 {
        "–".to_owned()
    } else {
        thousands(number)
    }
}

/// Writes `1205` as `1,205`.
///
/// # Examples
///
/// ```
/// use glurmo_model::units::thousands;
///
/// assert_eq!(thousands(1_234_567), "1,234,567");
/// ```
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
///
/// # Examples
///
/// ```
/// use glurmo_model::units::span;
///
/// assert_eq!(span(3_599), "59m");
/// ```
pub fn span(seconds: i64) -> String {
    let seconds = seconds.max(0);
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// Writes a stretch of time left in whole hours up to two days, then as [`span`] does, so a day
/// reads as `24h` beside `4h` and `8h`.
///
/// # Examples
///
/// ```
/// use glurmo_model::units::horizon;
///
/// assert_eq!(horizon(86_400), "24h");
/// assert_eq!(horizon(3 * 86_400), "3d");
/// ```
pub fn horizon(seconds: i64) -> String {
    match seconds {
        3_600..172_800 => format!("{}h", seconds / 3_600),
        _ => span(seconds),
    }
}

/// How long ago `then` was at `now`.
///
/// # Examples
///
/// ```
/// use glurmo_model::UnixTime;
/// use glurmo_model::units::since;
///
/// assert_eq!(since(UnixTime(0), UnixTime(7_200)), "2h");
/// ```
pub fn since(then: UnixTime, now: UnixTime) -> String {
    span(now.0 - then.0)
}

/// Writes a load average in five characters or fewer: `0.06`, `48.6`, `378`.
///
/// # Examples
///
/// ```
/// use glurmo_model::units::load;
///
/// assert_eq!(load(48.62), "48.6");
/// ```
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
///
/// # Examples
///
/// ```
/// use glurmo_model::units::percent;
///
/// assert_eq!(percent(1, 3), 33);
/// assert_eq!(percent(1, 0), 0);
/// ```
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
    }

    #[test]
    fn memory_under_a_gigabyte_reads_in_megabytes() {
        assert_eq!(gigabytes(0), "0M");
        assert_eq!(gigabytes(512), "512M");
        assert_eq!(gigabytes(1_023), "1023M");
        assert_eq!(gigabytes(1_024), "1G");
    }
}
