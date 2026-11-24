//! The refusal report a pre-receive hook writes: one `<code> <rule>` line per refusal.
#![cfg_attr(not(test), allow(dead_code))]

pub(crate) const MAX_LINES: usize = 64;
pub(crate) const MAX_BYTES: usize = 8 * 1024;

/// What a receive-pack's hook reported: whether it refused, and the (code, rule) pairs it gave.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct HookReport {
    refused: bool,
    refusals: Vec<(&'static str, String)>,
}

impl HookReport {
    pub(crate) fn refused(&self) -> bool {
        self.refused
    }

    /// (code, rule): code is a known code or "unknown"; rule matches [a-z0-9_]{1,64}.
    pub(crate) fn refusals(&self) -> &[(&'static str, String)] {
        &self.refusals
    }
}

/// Empty input: not refused. Otherwise refused; the first 64 lines of the first 8 KiB that are
/// exactly two space-separated fields matching [a-z0-9_]{1,64} become refusals (a code outside
/// the known set becomes "unknown"); none valid gives one ("unknown", "unknown").
pub(crate) fn parse(bytes: &[u8]) -> HookReport {
    if bytes.is_empty() {
        return HookReport::default();
    }
    // The file is hook-written and untrusted: read a bounded prefix, keep only well-formed lines,
    // and fold codes outside the known set into "unknown" so labels stay bounded.
    let head = &bytes[..bytes.len().min(MAX_BYTES)];
    let mut refusals: Vec<(&'static str, String)> = head
        .split(|&b| b == b'\n')
        .take(MAX_LINES)
        .filter_map(|line| {
            let line = std::str::from_utf8(line).ok()?;
            let (code, rule) = line.split_once(' ')?;
            if !is_field(code) || !is_field(rule) {
                return None;
            }
            let code = KNOWN_CODES
                .iter()
                .find(|known| **known == code)
                .copied()
                .unwrap_or("unknown");
            Some((code, rule.to_owned()))
        })
        .collect();
    if refusals.is_empty() {
        refusals.push(("unknown", "unknown".to_owned()));
    }
    HookReport {
        refused: true,
        refusals,
    }
}

const KNOWN_CODES: [&str; 6] = [
    "secret_env_file",
    "secret_value",
    "secret_pattern",
    "format_unsupported",
    "safety_relaxation",
    "hook_error",
];

/// A field is 1 to 64 bytes of `[a-z0-9_]`.
fn is_field(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(r: &HookReport) -> Vec<(&str, &str)> {
        r.refusals().iter().map(|(c, r)| (*c, r.as_str())).collect()
    }

    #[test]
    fn an_empty_report_is_no_refusal() {
        let r = parse(b"");
        assert!(!r.refused());
        assert!(r.refusals().is_empty());
    }

    #[test]
    fn valid_lines_count_and_unknown_codes_are_unknown() {
        let r = parse(
            b"secret_env_file env_file\nsecret_pattern aws\nnew_code some_rule\nhook_error none\n",
        );
        assert!(r.refused());
        assert_eq!(
            pairs(&r),
            [
                ("secret_env_file", "env_file"),
                ("secret_pattern", "aws"),
                ("unknown", "some_rule"),
                ("hook_error", "none")
            ]
        );
    }

    #[test]
    fn malformed_lines_are_dropped() {
        let long = "a".repeat(65);
        let text = format!(
            "secret_value one two\nSECRET_VALUE x\nsecret_value {long}\nsecret_value\n\u{0}\u{1} x\nsecret_value bad-dash\nsecret_value ok_rule\n"
        );
        let r = parse(text.as_bytes());
        assert_eq!(pairs(&r), [("secret_value", "ok_rule")]);
    }

    #[test]
    fn a_report_with_no_valid_line_counts_once_as_unknown() {
        let r = parse(b"\xff\xfe garbage\n");
        assert!(r.refused());
        assert_eq!(pairs(&r), [("unknown", "unknown")]);
    }

    #[test]
    fn only_64_lines_of_the_first_8_kib_are_read() {
        let lines: String = (0..70).map(|i| format!("secret_pattern r{i}\n")).collect();
        assert_eq!(parse(lines.as_bytes()).refusals().len(), MAX_LINES);
        let mut big = vec![b'x'; MAX_BYTES];
        big.extend_from_slice(b"\nsecret_pattern late\n");
        let r = parse(&big);
        assert_eq!(
            pairs(&r),
            [("unknown", "unknown")],
            "the line after 8 KiB is never read"
        );
    }
}
