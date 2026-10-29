use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};

fn run(db: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_todo"))
        .arg("--database")
        .arg(db)
        .arg("--json")
        .args(args)
        .output()
        .unwrap()
}

fn ok(db: &Path, args: &[&str]) -> Value {
    let output = run(db, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn json_api_handles_project_and_personal_task_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("tasks.sqlite3");
    let project = ok(&db, &["project", "add", "Website"]);
    let project_id = project["id"].to_string();
    let task = ok(
        &db,
        &[
            "task",
            "add",
            "Build page",
            "--project",
            &project_id,
            "--notes",
            "Write copy\nReview",
            "--priority",
            "high",
        ],
    );
    let id = task["id"].to_string();
    assert_eq!(task["project_name"], "Website");
    assert_eq!(
        ok(&db, &["task", "list", "--personal"]),
        serde_json::json!([])
    );
    assert_eq!(
        ok(&db, &["task", "list", "--search", "copy"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    ok(&db, &["task", "edit", &id, "--title", "Build website"]);
    let task = ok(&db, &["task", "show", &id]);
    assert_eq!(task["notes"], "Write copy\nReview");
    assert_eq!(task["priority"], "high");
    assert_eq!(task["project_id"].to_string(), project_id);
    ok(&db, &["task", "done", &id]);
    let completed = ok(&db, &["task", "show", &id]);
    let closed_at = completed["completed_at"].as_str().unwrap().to_owned();
    assert!(chrono::DateTime::parse_from_rfc3339(&closed_at).is_ok());
    assert_eq!(ok(&db, &["task", "done", &id])["completed_at"], closed_at);
    assert_eq!(ok(&db, &["task", "list"]), serde_json::json!([]));
    assert_eq!(
        ok(&db, &["task", "list", "--status", "done"])[0]["id"].to_string(),
        id
    );
    ok(&db, &["task", "reopen", &id]);
    assert!(ok(&db, &["task", "show", &id])["completed_at"].is_null());
    ok(&db, &["project", "rename", &project_id, "New name"]);
    assert_eq!(ok(&db, &["task", "show", &id])["project_name"], "New name");
    ok(&db, &["project", "delete", &project_id]);
    assert!(ok(&db, &["task", "show", &id])["project_id"].is_null());
    ok(&db, &["task", "edit", &id, "--notes", ""]);
    assert_eq!(ok(&db, &["task", "show", &id])["notes"], "");
    ok(&db, &["task", "delete", &id]);
    assert_eq!(
        ok(&db, &["task", "list", "--status", "all"]),
        serde_json::json!([])
    );
}

#[test]
fn runtime_errors_are_json_and_workspaces_are_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("one.sqlite3");
    let other = dir.path().join("two.sqlite3");
    ok(&db, &["task", "add", "Private"]);
    assert_eq!(ok(&other, &["task", "list"]), serde_json::json!([]));
    for args in [
        vec!["task", "show", "999"],
        vec!["task", "add", " "],
        vec!["task", "edit", "1"],
        vec!["task", "add", "Wrong project", "--project", "999"],
    ] {
        let output = run(&db, &args);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert!(error["error"].as_str().unwrap().len() > 5);
    }
}

#[test]
fn concurrent_clients_create_tasks_without_losing_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("tasks.sqlite3");
    ok(&db, &["project", "list"]);
    let children: Vec<_> = (0..6)
        .map(|i| {
            Command::new(env!("CARGO_BIN_EXE_todo"))
                .arg("--database")
                .arg(&db)
                .args(["--json", "task", "add", &format!("Task {i}")])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    assert_eq!(ok(&db, &["task", "list"]).as_array().unwrap().len(), 6);
}

#[test]
fn deadlines_and_in_progress_are_available_through_json_api() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("tasks.sqlite3");
    let task = ok(&db, &["task", "add", "Report", "--deadline", "20280229"]);
    let id = task["id"].to_string();
    assert_eq!(task["deadline"], "2028-02-29");
    assert_eq!(task["in_progress"], false);
    assert_eq!(ok(&db, &["task", "start", &id])["in_progress"], true);
    assert_eq!(
        ok(&db, &["task", "list", "--status", "in-progress"])[0]["id"].to_string(),
        id
    );
    ok(&db, &["task", "edit", &id, "--title", "Renamed"]);
    let preserved = ok(&db, &["task", "show", &id]);
    assert_eq!(preserved["deadline"], "2028-02-29");
    assert_eq!(preserved["in_progress"], true);
    ok(&db, &["task", "edit", &id, "--deadline", "20280301"]);
    let invalid = run(&db, &["task", "edit", &id, "--deadline", "20270229"]);
    assert!(!invalid.status.success());
    assert!(
        serde_json::from_slice::<Value>(&invalid.stderr).unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("YYYY-MM-DD")
    );
    assert_eq!(ok(&db, &["task", "show", &id])["deadline"], "2028-03-01");
    assert_eq!(ok(&db, &["task", "pause", &id])["in_progress"], false);
    assert_eq!(
        ok(&db, &["task", "list", "--status", "in-progress"]),
        serde_json::json!([])
    );
    ok(&db, &["task", "start", &id]);
    let done = ok(&db, &["task", "done", &id]);
    assert_eq!(done["done"], true);
    assert_eq!(done["in_progress"], false);
    let restarted = ok(&db, &["task", "start", &id]);
    assert_eq!(restarted["done"], false);
    assert_eq!(restarted["in_progress"], true);
    assert!(restarted["completed_at"].is_null());
    let cleared = ok(&db, &["task", "edit", &id, "--no-deadline"]);
    assert!(cleared["deadline"].is_null());
    assert!(!run(
        &db,
        &[
            "task",
            "edit",
            &id,
            "--deadline",
            "2028-03-01",
            "--no-deadline"
        ]
    )
    .status
    .success());
}

#[test]
fn start_dates_validate_clear_and_preserve_local_task_fields() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("tasks.sqlite3");
    let task = ok(
        &db,
        &[
            "task",
            "add",
            "Plan",
            "--start-date",
            "20260601",
            "--deadline",
            "20260615",
        ],
    );
    assert_eq!(task["start_date"], "2026-06-01");
    assert_eq!(task["deadline"], "2026-06-15");
    assert!(task["assignees"].as_array().unwrap().is_empty());
    let id = task["id"].to_string();
    assert_eq!(
        ok(&db, &["task", "edit", &id, "--start-date", "20260602"])["start_date"],
        "2026-06-02"
    );
    assert!(
        !run(&db, &["task", "edit", &id, "--start-date", "20260229"])
            .status
            .success()
    );
    assert_eq!(ok(&db, &["task", "show", &id])["start_date"], "2026-06-02");
    assert!(ok(&db, &["task", "edit", &id, "--no-start-date"])["start_date"].is_null());
    assert!(
        !run(&db, &["task", "add", "Invalid", "--start-date", "bad"])
            .status
            .success()
    );
    assert_eq!(ok(&db, &["task", "list"]).as_array().unwrap().len(), 1);
    assert_eq!(
        ok(&db, &["clickup", "auto", "--disable"])["automatic_sync"],
        false
    );
    assert_eq!(
        ok(
            &db,
            &[
                "clickup",
                "auto",
                "--enable",
                "--timezone",
                "Europe/Stockholm"
            ]
        )["automatic_sync"],
        true
    );
}
