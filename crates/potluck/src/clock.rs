//! RFC 3339 UTC timestamps to the second, in one form so stored stamps compare as strings.

use std::time::Duration;

use jiff::Timestamp;

/// Now as RFC 3339 UTC to the second: `2026-11-15T10:04:05Z`.
pub fn now_rfc3339() -> String {
    render(Timestamp::now())
}

/// Any RFC 3339 instant (offset, fraction) rendered in the `now_rfc3339` form, so stored stamps
/// compare chronologically as strings. `None` when it does not parse or has no offset.
pub fn normalize_rfc3339(input: &str) -> Option<String> {
    input.parse::<Timestamp>().ok().map(render)
}

/// `stamp` moved by `secs` (negative for earlier), in the same form; `None` for a bad stamp.
pub fn shift_secs(stamp: &str, secs: i64) -> Option<String> {
    let ts = stamp.parse::<Timestamp>().ok()?;
    let shifted = Timestamp::from_second(ts.as_second().checked_add(secs)?).ok()?;
    Some(render(shifted))
}

/// The time from now until the RFC 3339 instant `stamp`: `Duration::ZERO` once it has passed,
/// `None` when it does not parse.
pub fn until(stamp: &str) -> Option<Duration> {
    let left = Timestamp::now().duration_until(stamp.parse::<Timestamp>().ok()?);
    // A negative span does not convert, and a past instant leaves no time at all.
    Some(Duration::try_from(left).unwrap_or(Duration::ZERO))
}

// A precision of 0 truncates the fraction, so every stamp is exactly 20 characters.
fn render(ts: Timestamp) -> String {
    format!("{ts:.0}")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn until_measures_from_now_and_floors_at_zero() {
        // A stamp is whole seconds, so one shifted 60 s ahead lies 59 to 60 s out.
        let ahead = shift_secs(&now_rfc3339(), 60).unwrap();
        let left = until(&ahead).unwrap();
        assert!(
            left > Duration::from_secs(58) && left <= Duration::from_secs(60),
            "{left:?}"
        );
        let past = shift_secs(&now_rfc3339(), -60).unwrap();
        assert_eq!(until(&past), Some(Duration::ZERO));
        assert_eq!(until("not a time"), None);
    }
}
