//! `potluck admin update check`: ask the server to check for a newer signed release now.

use serde_json::Value;

use super::client::Verb;
use super::output::escape;
use super::{AdminError, Context};
use crate::update::IMAGE;

/// The `update` verbs.
#[derive(clap::Subcommand, Debug)]
#[non_exhaustive]
pub enum UpdateCmd {
    /// Check now and print the update steps pinned to the release's digest.
    Check,
}

pub(crate) async fn run(cmd: UpdateCmd, cx: &mut Context<'_>) -> Result<(), AdminError> {
    match cmd {
        UpdateCmd::Check => {
            cx.call(Verb::Post, "/api/v1/update/check", None, &[200], report)
                .await
        }
    }
}

/// Every field comes from the server and the release feed, so it is escaped before printing.
fn text(value: &Value, key: &str) -> String {
    escape(value.get(key).and_then(Value::as_str).unwrap_or(""))
}

/// The update object for a person: one line when up to date, else the release and its steps.
fn report(body: &Value) -> String {
    let current = text(body, "current");
    match body.get("latest").filter(|latest| latest.is_object()) {
        Some(latest) => steps(&current, latest),
        None => {
            let checked = escape(
                body.get("checked_at")
                    .and_then(Value::as_str)
                    .unwrap_or("never"),
            );
            format!("potluck {current} is up to date (checked {checked})\n")
        }
    }
}

/// Every step names the image by digest, formatted once, so each command fetches exactly the
/// release the server verified rather than whatever a tag points at by then.
fn steps(current: &str, latest: &Value) -> String {
    let version = text(latest, "version");
    let published = text(latest, "published");
    let notes = text(latest, "notes_url");
    let pinned = format!("{IMAGE}@{}", text(latest, "digest"));
    format!(
        "potluck {version} is available (running {current})\n\
         published: {published}\n\
         notes:     {notes}\n\
         image:     {pinned}\n\
         \n\
         Every step names the digest, so you get exactly the release the server verified.\n\
         \n\
         Quadlet:\n  \
           podman pull {pinned}\n  \
           podman auto-update\n  \
           auto-update follows the unit's tag; to pin this release, set Image={pinned} in the unit and restart it.\n\
         \n\
         Compose:\n  \
           set image: {pinned} in compose.yaml, then run docker compose up -d (or podman compose up -d)\n\
         \n\
         podman run or docker run:\n  \
           podman pull {pinned}\n  \
           podman rm -f potluck, then podman run as in the README's Updating section with {pinned} as the image (docker takes the same words)\n\
         \n\
         Verify the image:\n  \
           gh attestation verify oci://{pinned} --repo PhongGuy/potluck\n"
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::report;

    #[test]
    fn an_available_release_escapes_every_feed_field() {
        let out = report(&json!({
            "current": "1.0\u{1b}[0m",
            "latest": {
                "version": "9\u{1b}[31m",
                "published": "2026\u{202E}",
                "notes_url": "https://x/\u{1b}]8;;",
                "digest": "sha256:\u{202E}ab",
            },
        }));
        assert!(
            !out.contains('\u{1b}') && !out.contains('\u{202E}'),
            "{out:?}"
        );
        for needle in [
            "potluck 9\\u{1b}[31m is available (running 1.0\\u{1b}[0m)\n",
            "published: 2026\\u{202e}\n",
            "notes:     https://x/\\u{1b}]8;;\n",
            "@sha256:\\u{202e}ab\n",
        ] {
            assert!(out.contains(needle), "missing {needle:?} in {out:?}");
        }
    }

    #[test]
    fn an_up_to_date_report_escapes_the_version_and_check_time() {
        let out = report(&json!({
            "current": "1.0\u{1b}[0m",
            "checked_at": "now\u{202E}",
        }));
        assert_eq!(
            out,
            "potluck 1.0\\u{1b}[0m is up to date (checked now\\u{202e})\n"
        );
    }
}
