//! Text tables for the admin command's non-JSON output.

use serde_json::Value;

/// A text table: `headers` then `rows`, each cell padded to its column's widest cell (by
/// `chars().count()`), two spaces between columns, no trailing spaces, every line `\n`-ended.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let header: Vec<String> = headers.iter().map(|h| (*h).to_owned()).collect();
    let mut text = String::new();
    for row in std::iter::once(&header).chain(rows) {
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
