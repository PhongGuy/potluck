//! Text tables for the admin command's non-JSON output.

use serde_json::Value;

/// A text table: `headers` then `rows`, every cell escaped (control and Unicode format
/// characters in their `\u{..}` form, a backslash doubled) and padded to its column's widest
/// escaped cell (by `chars().count()`), two spaces between columns, no trailing spaces, every
/// line `\n`-ended.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let rows: Vec<Vec<String>> = std::iter::once(headers.iter().map(|h| escape(h)).collect())
        .chain(
            rows.iter()
                .map(|row| row.iter().map(|c| escape(c)).collect()),
        )
        .collect();
    let mut widths: Vec<usize> = vec![0; headers.len()];
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut text = String::new();
    for row in &rows {
        let mut line = String::new();
        for (cell, width) in row.iter().zip(&widths) {
            line.push_str(cell);
            line.extend(std::iter::repeat_n(' ', width - cell.chars().count() + 2));
        }
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text
}

/// `USERNAME  DISPLAY NAME  EMAIL  ADMIN  DISABLED  CREATED` rows from User objects.
pub(crate) fn users(items: &[Value]) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|u| {
            vec![
                text(&u["username"]),
                text(&u["display_name"]),
                text(&u["email"]),
                flag(&u["server_admin"]),
                flag(&u["disabled"]),
                text(&u["created_at"]),
            ]
        })
        .collect();
    table(
        &[
            "USERNAME",
            "DISPLAY NAME",
            "EMAIL",
            "ADMIN",
            "DISABLED",
            "CREATED",
        ],
        &rows,
    )
}

/// `ID  NAME  PREFIX  CREATED  LAST USED  EXPIRES` rows from Token objects.
pub(crate) fn tokens(items: &[Value]) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|t| {
            vec![
                text(&t["id"]),
                text(&t["name"]),
                text(&t["prefix"]),
                text(&t["created_at"]),
                text(&t["last_used_at"]),
                text(&t["expires_at"]),
            ]
        })
        .collect();
    table(
        &["ID", "NAME", "PREFIX", "CREATED", "LAST USED", "EXPIRES"],
        &rows,
    )
}

/// A cell: strings as sent, null or missing as `-`, anything else in its JSON form.
fn text(value: &Value) -> String {
    match value {
        Value::Null => "-".to_owned(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn flag(value: &Value) -> String {
    match value {
        Value::Bool(true) => "yes".to_owned(),
        Value::Bool(false) => "no".to_owned(),
        other => text(other),
    }
}

/// `SLUG  NAME  MEMBERS` rows from Team objects, members comma-joined, `-` when none.
pub(crate) fn teams(items: &[Value]) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|t| {
            let members: Vec<String> = t["members"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(text)
                .collect();
            let members = if members.is_empty() {
                "-".to_owned()
            } else {
                members.join(",")
            };
            vec![text(&t["slug"]), text(&t["name"]), members]
        })
        .collect();
    table(&["SLUG", "NAME", "MEMBERS"], &rows)
}

/// `WORKSPACE  NAME  KIND  BRANCH  ROLE  GIT URL` rows from Workspace objects, the first
/// column as `{team}/{slug}`.
pub(crate) fn workspaces(items: &[Value]) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|w| {
            vec![
                format!("{}/{}", text(&w["team"]), text(&w["slug"])),
                text(&w["name"]),
                text(&w["kind"]),
                text(&w["default_branch"]),
                text(&w["role"]),
                text(&w["git_url"]),
            ]
        })
        .collect();
    table(
        &["WORKSPACE", "NAME", "KIND", "BRANCH", "ROLE", "GIT URL"],
        &rows,
    )
}

/// `SUBJECT  ROLE` rows from Grant objects, the subject as `{kind}:{name}`.
pub(crate) fn grants(items: &[Value]) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|g| {
            let subject = &g["subject"];
            vec![
                format!("{}:{}", text(&subject["kind"]), text(&subject["name"])),
                text(&g["role"]),
            ]
        })
        .collect();
    table(&["SUBJECT", "ROLE"], &rows)
}

/// `ID  FOR USER  REASON  ARMED BY  ARMED AT  CONSUMED AT  REFS` rows from Override objects;
/// REFS is how many refs the consuming push moved, `-` while armed.
pub(crate) fn overrides(items: &[Value]) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|o| {
            let refs = o["consumed_refs"]
                .as_array()
                .map_or_else(|| "-".to_owned(), |moves| moves.len().to_string());
            vec![
                text(&o["id"]),
                text(&o["for_user"]),
                text(&o["reason"]),
                text(&o["armed_by"]),
                text(&o["armed_at"]),
                text(&o["consumed_at"]),
                refs,
            ]
        })
        .collect();
    table(
        &[
            "ID",
            "FOR USER",
            "REASON",
            "ARMED BY",
            "ARMED AT",
            "CONSUMED AT",
            "REFS",
        ],
        &rows,
    )
}

/// `ID  TIME  ACTOR  ACTION  TARGET  WORKSPACE  IP` rows from Audit objects.
/// ACTOR is the actor's name, else its kind; TARGET is `kind:name`, else `kind:#id`, else the
/// kind alone, else `-`.
pub(crate) fn audit(items: &[Value]) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|a| {
            let actor = if a["actor_name"].is_null() {
                text(&a["actor_kind"])
            } else {
                text(&a["actor_name"])
            };
            let target = match (&a["target_kind"], &a["target_name"], &a["target_id"]) {
                (Value::Null, _, _) => "-".to_owned(),
                (kind, Value::Null, Value::Null) => text(kind),
                (kind, Value::Null, id) => format!("{}:#{}", text(kind), text(id)),
                (kind, name, _) => format!("{}:{}", text(kind), text(name)),
            };
            vec![
                text(&a["id"]),
                text(&a["at"]),
                actor,
                text(&a["action"]),
                target,
                text(&a["workspace"]),
                text(&a["client_ip"]),
            ]
        })
        .collect();
    table(
        &["ID", "TIME", "ACTOR", "ACTION", "TARGET", "WORKSPACE", "IP"],
        &rows,
    )
}

// Table cells carry names other people chose, so a control character or an invisible format
// character (bidi override, zero-width) is shown in its escaped form rather than reaching the
// terminal as a command or a reordering of the line. A backslash is doubled so a name that
// literally reads `\u{1b}` prints differently from an escaped ESC.
pub(crate) fn escape(cell: &str) -> String {
    cell.chars()
        .map(|c| {
            if c == '\\' || c.is_control() || is_format(c) {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// Whether `c` is in Unicode general category Cf (format).
fn is_format(c: char) -> bool {
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
