//! The Mac's first-launch checklist (#250): the steps `setup::first_launch`
//! couldn't do, in the window's setup checklist (index.html, `onboarding`),
//! as Linux shows its own. Each has Try again; the ones Kiri works without
//! have Skip, which lasts until the app next opens.

use std::sync::Mutex;

use serde_json::{Map, Value, json};

use crate::setup::FIRST_STEPS;

/// Steps Kiri works without: the commands (docker reaches Kiri through its
/// context either way) and starting at login (the window starts the engine).
const OPTIONAL: [&str; 2] = ["commands", "login"];

static SKIPPED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// `{type: "onboarding", os: "mac", ready, complete, skipped, checks, errors}`.
pub fn checks() -> Value {
    event(&crate::setup::undone(), &SKIPPED.lock().unwrap_or_else(|e| e.into_inner()))
}

fn event(undone: &[(&str, String)], skipped: &[String]) -> Value {
    let (mut checks, mut errors) = (Map::new(), Map::new());
    for step in FIRST_STEPS {
        match undone.iter().find(|(s, _)| *s == step) {
            Some((_, e)) => {
                checks.insert(step.into(), "no".into());
                errors.insert(step.into(), e.clone().into());
            }
            None => {
                checks.insert(step.into(), "ok".into());
            }
        }
    }
    let done = |k: &str| checks[k] == "ok" || (OPTIONAL.contains(&k) && skipped.iter().any(|s| s == k));
    let complete = FIRST_STEPS.iter().all(|k| done(k));
    json!({"type": "onboarding", "os": "mac", "ready": checks["context"] == "ok", "complete": complete, "skipped": skipped, "checks": checks, "errors": errors})
}

/// Whether to show the checklist: a step is undone and not skipped.
pub fn needed(checks: &Value) -> bool {
    checks["complete"] != true
}

/// Skips an optional step, or takes the skip back (`undo`).
pub fn skip(what: &str, undo: bool) {
    let mut list = SKIPPED.lock().unwrap_or_else(|e| e.into_inner());
    list.retain(|s| s != what);
    if !undo && OPTIONAL.contains(&what) {
        list.push(what.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::event;

    #[test]
    fn only_the_optional_steps_can_be_skipped_past() {
        let ok = event(&[], &[]);
        assert_eq!((ok["complete"].as_bool(), ok["checks"]["login"].as_str()), (Some(true), Some("ok")));
        let login = [("login", "launchctl could not load the agent".to_owned())];
        let c = event(&login, &[]);
        assert_eq!((c["complete"].as_bool(), c["ready"].as_bool()), (Some(false), Some(true)));
        assert_eq!(c["errors"]["login"], "launchctl could not load the agent");
        assert_eq!(event(&login, &["login".into()])["complete"], true, "skipped");
        let context = [("context", "~/.docker: File exists".to_owned())];
        let c = event(&context, &["context".into()]);
        assert_eq!((c["complete"].as_bool(), c["ready"].as_bool()), (Some(false), Some(false)), "docker can't reach Kiri: no skipping that");
    }
}
