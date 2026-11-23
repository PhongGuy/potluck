//! `potluck admin audit`: the table, escaping, JSON and the query it sends.

use potluck::clock::{now_rfc3339, shift_secs};
use serde_json::Value;

use crate::common::{admin, admin_server, columns};

/// Ten days ago plus `minutes`: recent enough that the startup pruner keeps it.
fn stamp(minutes: i64) -> String {
    let base = shift_secs(&now_rfc3339(), -10 * 86_400).unwrap();
    shift_secs(&base, 60 * minutes).unwrap()
}

fn seed(dir: &std::path::Path, rows: &[(&str, &str, Option<&str>)]) {
    let c = rusqlite::Connection::open(dir.join("server.db")).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    for (at, actor, target) in rows {
        c.execute(
            "INSERT INTO audit (at, action, actor_kind, actor_name, target_kind, target_name, \
             workspace, client_ip) VALUES (?1, 'team.member_add', 'user', ?2, 'team', ?3, \
             'core/api', '192.0.2.9')",
            rusqlite::params![at, actor, target],
        )
        .unwrap();
    }
}

#[tokio::test]
async fn the_table_has_its_columns_and_escapes_control_characters() {
    let (dir, _running) = admin_server().await;
    let at = stamp(0);
    seed(dir.path(), &[(&at, "bob", Some("core\u{1b}[2Jx"))]);
    let (result, out) = admin(dir.path(), &["audit"], "").await;
    result.unwrap();
    let mut lines = out.lines();
    assert_eq!(
        columns(lines.next().unwrap()),
        ["ID", "TIME", "ACTOR", "ACTION", "TARGET", "WORKSPACE", "IP"],
        "{out}"
    );
    let row = lines.next().unwrap();
    assert!(
        !out.contains('\u{1b}'),
        "no raw escape reaches the terminal: {out:?}"
    );
    assert!(row.contains("team:core\\u{1b}[2Jx"), "{row}");
    assert!(row.contains(&at) && row.contains("bob"), "{row}");
    assert!(
        row.contains("core/api") && row.contains("192.0.2.9"),
        "{row}"
    );
}

#[tokio::test]
async fn json_prints_the_raw_array_and_flags_filter() {
    let (dir, _running) = admin_server().await;
    let (first, second, third) = (stamp(0), stamp(5), stamp(10));
    seed(
        dir.path(),
        &[
            (&first, "bob", Some("core")),
            (&second, "ann", Some("core")),
            (&third, "bob", None),
        ],
    );
    let (result, out) = admin(dir.path(), &["--json", "audit", "--actor", "bob"], "").await;
    result.unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    let rows = v.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r["actor_name"] == "bob"));
    // An offset stamp must reach the server with its `+` intact: four minutes past the first
    // row, written as the +01:00 wall clock of that instant.
    let local = shift_secs(&stamp(4), 3600).unwrap();
    let since = format!("{}+01:00", local.trim_end_matches('Z'));
    let (result, out) = admin(
        dir.path(),
        &["--json", "audit", "--since", &since, "--limit", "1"],
        "",
    )
    .await;
    result.unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["at"], third.as_str(), "newest first");
}

#[tokio::test]
async fn target_cells_cover_kind_alone_id_and_none() {
    let (dir, _running) = admin_server().await;
    let (first, second, third) = (stamp(0), stamp(1), stamp(2));
    seed(dir.path(), &[(&first, "bob", None)]);
    {
        let c = rusqlite::Connection::open(dir.path().join("server.db")).unwrap();
        c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
        c.execute(
            "INSERT INTO audit (at, action, actor_kind, target_kind, target_id) \
             VALUES (?1, 'token.revoke', 'system', 'token', 12)",
            [&second],
        )
        .unwrap();
        c.execute(
            "INSERT INTO audit (at, action, actor_kind) VALUES (?1, 'team.create', 'system')",
            [&third],
        )
        .unwrap();
    }
    let (result, out) = admin(dir.path(), &["audit"], "").await;
    result.unwrap();
    let lines: Vec<&str> = out.lines().collect();
    // Newest first: no target, then token by id, then team without name or id.
    assert_eq!(
        columns(lines[1])[4],
        "-",
        "no target is a dash in the TARGET column: {}",
        lines[1]
    );
    assert!(
        columns(lines[1]).contains(&"system"),
        "a nameless system actor shows its name: {}",
        lines[1]
    );
    assert!(columns(lines[2]).contains(&"token:#12"), "{}", lines[2]);
    assert!(
        columns(lines[3]).contains(&"team"),
        "a kind alone renders alone: {}",
        lines[3]
    );
}
