//! Times written in the view's time zone.

use jiff::tz::TimeZone;

use rtviz_model::UnixTime;

/// Writes `at` in `tz` with a `strftime` format.
pub fn clock(at: UnixTime, tz: &TimeZone, format: &str) -> String {
    jiff::Timestamp::from_second(at.0).map_or_else(
        |_| String::new(),
        |time| time.to_zoned(tz.clone()).strftime(format).to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clocks_read_in_the_zone_given() {
        assert_eq!(clock(UnixTime(86_399), &TimeZone::UTC, "%H:%M"), "23:59");
    }
}
