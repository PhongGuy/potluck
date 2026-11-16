//! Length checks shared by the handlers that take free-text names.

use super::error::ApiError;

/// `s` trimmed, when it is 1 to 200 characters; else 422 naming `field`:
/// "<field> must be 1 to 200 characters".
pub(super) fn name<'a>(field: &'static str, s: &'a str) -> Result<&'a str, ApiError> {
    within(field, s, 200)
}

/// `s` trimmed, when it is 1 to 100 characters; else 422 naming `field`:
/// "<field> must be 1 to 100 characters".
pub(super) fn token_name<'a>(field: &'static str, s: &'a str) -> Result<&'a str, ApiError> {
    within(field, s, 100)
}

/// `s` trimmed, when it is 1 to `max` characters; else 422 naming `field`.
/// The limit counts characters, not bytes, so a non-ASCII name gets the same room.
fn within<'a>(field: &'static str, s: &'a str, max: usize) -> Result<&'a str, ApiError> {
    let trimmed = s.trim();
    if (1..=max).contains(&trimmed.chars().count()) {
        Ok(trimmed)
    } else {
        Err(ApiError::invalid_value(
            field,
            format!("{field} must be 1 to {max} characters"),
        ))
    }
}
