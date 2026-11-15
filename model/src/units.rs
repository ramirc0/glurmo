//! Numbers and durations written short, for narrow columns.

use crate::UnixTime;

/// Writes `1205` as `1,205`.
///
/// # Examples
///
/// ```
/// use rtviz_model::units::thousands;
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
/// use rtviz_model::units::span;
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

/// How long ago `then` was at `now`.
///
/// # Examples
///
/// ```
/// use rtviz_model::UnixTime;
/// use rtviz_model::units::since;
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
/// use rtviz_model::units::load;
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
/// use rtviz_model::units::percent;
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
}
