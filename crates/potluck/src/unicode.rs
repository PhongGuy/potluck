//! Character classes shared by every place that prints text other people chose.

use std::borrow::Cow;

/// `text` with every backslash, control character, format character and line or paragraph
/// separator replaced by its Rust escape (`\\`, `\n`, `\u{202e}`), so text other people chose
/// can neither steer a terminal nor start a new line. Borrowed when nothing needs escaping.
pub(crate) fn inert(text: &str) -> Cow<'_, str> {
    // A doubled backslash keeps a name that literally reads `\u{1b}` apart from an escaped ESC.
    let risky = |c: char| {
        c == '\\' || c.is_control() || is_format(c) || matches!(c, '\u{2028}' | '\u{2029}')
    };
    if !text.contains(risky) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if risky(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Whether `c` is in Unicode general category Cf (format).
pub(crate) fn is_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// `err` and each of its sources joined by `": "`, the way anyhow's `{:#}` renders a chain,
/// then made [`inert`]: an error's text may quote git stderr, paths or pushed names.
pub(crate) fn inert_chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    inert(&text).into_owned()
}

#[cfg(test)]
mod tests {
    use super::inert;
    use std::borrow::Cow;

    #[test]
    fn printable_text_is_borrowed_unchanged() {
        let text = "café 名前 'quoted' \"double\" tab-free";
        assert!(matches!(inert(text), Cow::Borrowed(t) if t == text));
    }

    #[test]
    fn a_bidi_override_is_escaped() {
        assert_eq!(inert("eve\u{202E}gnp.exe"), r"eve\u{202e}gnp.exe");
    }

    #[test]
    fn controls_are_escaped() {
        assert_eq!(inert("a\nb\r\t\x1b\u{85}"), r"a\nb\r\t\u{1b}\u{85}");
    }

    #[test]
    fn line_and_paragraph_separators_are_escaped() {
        assert_eq!(inert("a\u{2028}b\u{2029}"), r"a\u{2028}b\u{2029}");
    }

    #[derive(Debug)]
    struct Outer(std::io::Error);

    impl std::fmt::Display for Outer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("cannot list refs")
        }
    }

    impl std::error::Error for Outer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn a_chain_names_each_cause_once_and_escapes_it() {
        let err = Outer(std::io::Error::other("ref eve\u{202E}gnp"));
        assert_eq!(
            super::inert_chain(&err),
            r"cannot list refs: ref eve\u{202e}gnp"
        );
    }

    #[test]
    fn a_literal_backslash_is_doubled() {
        assert_eq!(inert(r"\u{202e}"), r"\\u{202e}");
    }
}
