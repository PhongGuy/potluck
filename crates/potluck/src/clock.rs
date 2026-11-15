//! RFC 3339 UTC timestamps to the second, in one form so stored stamps compare as strings.

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

// A precision of 0 truncates the fraction, so every stamp is exactly 20 characters.
fn render(ts: Timestamp) -> String {
    format!("{ts:.0}")
}
