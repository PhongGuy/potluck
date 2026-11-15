//! The check over one push, and the lines the client sees.

use std::io::Write;

use crate::kinds::Finding;

/// What the server told the hook through its environment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HookEnv {
    kind: Option<String>,
    import: bool,
    override_id: Option<String>,
}

impl HookEnv {
    /// Reads `POTLUCK_KIND` (non-empty), `POTLUCK_IMPORT` (`1` means import) and
    /// `POTLUCK_OVERRIDE` (non-empty); an unset variable and an empty one mean the same.
    pub fn from_env() -> HookEnv {
        let text = |name: &str| std::env::var_os(name).map(|v| v.to_string_lossy().into_owned());
        HookEnv::new(
            text("POTLUCK_KIND").as_deref(),
            text("POTLUCK_IMPORT").as_deref() == Some("1"),
            text("POTLUCK_OVERRIDE").as_deref(),
        )
    }

    /// Builds the environment from its parts; an empty kind or override counts as none.
    pub fn new(kind: Option<&str>, import: bool, override_id: Option<&str>) -> HookEnv {
        let non_empty = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_owned);
        HookEnv {
            kind: non_empty(kind),
            import,
            override_id: non_empty(override_id),
        }
    }

    /// The workspace kind, if one was given.
    pub fn kind(&self) -> Option<&str> {
        self.kind.as_deref()
    }

    /// Whether this push is an import.
    pub fn is_import(&self) -> bool {
        self.import
    }

    /// The admin override id, if one applies to this push.
    pub fn override_id(&self) -> Option<&str> {
        self.override_id.as_deref()
    }
}

/// One refused path under one ref.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Rejection {
    ref_name: String,
    path: Vec<u8>,
    finding: Finding,
}

impl Rejection {
    /// A refusal of `path` under `ref_name` for `finding`.
    pub fn new(ref_name: &str, path: &[u8], finding: Finding) -> Rejection {
        Rejection {
            ref_name: ref_name.to_owned(),
            path: path.to_vec(),
            finding,
        }
    }

    /// The ref the path was pushed under.
    pub fn ref_name(&self) -> &str {
        &self.ref_name
    }

    /// The refused path, as git stores it.
    pub fn path(&self) -> &[u8] {
        &self.path
    }

    /// The rule that refused it.
    pub fn finding(&self) -> &Finding {
        &self.finding
    }
}

/// How many `potluck-reject` lines are printed at most.
pub const MAX_LINES: usize = 20;

/// `potluck-reject <code> <ref> <path> <rule>\n` for the first `MAX_LINES` rejections, then
/// `potluck-reject-more <n>\n` when `n` remain. The path and the ref are percent-encoded:
/// `%`, space, bytes below 0x20, 0x7f and every byte of an invalid UTF-8 sequence become
/// `%XX` (upper-case hex); every other byte is written as is. The line splits on spaces into
/// exactly five fields.
pub fn report(rejections: &[Rejection], out: &mut impl Write) -> std::io::Result<()> {
    for r in rejections.iter().take(MAX_LINES) {
        writeln!(
            out,
            "potluck-reject {} {} {} {}",
            r.finding.code().as_str(),
            percent_encode(r.ref_name.as_bytes()),
            percent_encode(&r.path),
            r.finding.rule(),
        )?;
    }
    let rest = rejections.len().saturating_sub(MAX_LINES);
    if rest > 0 {
        writeln!(out, "potluck-reject-more {rest}")?;
    }
    Ok(())
}

/// The encoding `report` applies to one field.
pub fn percent_encode(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len());
    // Valid UTF-8 stays readable except for the bytes that would break the line's fields;
    // each byte of an invalid sequence is escaped so the client can restore the exact path.
    for chunk in raw.utf8_chunks() {
        for c in chunk.valid().chars() {
            if c == '%' || c == ' ' || c < ' ' || c == '\x7f' {
                push_escape(&mut out, c as u8);
            } else {
                out.push(c);
            }
        }
        for &b in chunk.invalid() {
            push_escape(&mut out, b);
        }
    }
    out
}

fn push_escape(out: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push('%');
    out.push(char::from(HEX[usize::from(byte >> 4)]));
    out.push(char::from(HEX[usize::from(byte & 0x0f)]));
}
