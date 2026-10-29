use crate::store::{Assignee, Priority, Store};
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use fs2::FileExt;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs::OpenOptions, path::Path, process::Command as Process};

pub mod projects;
pub mod sources;

#[derive(Subcommand)]
pub enum Command {
    /// Sign in using GitHub CLI; choose sources from the ? menu
    Connect,
    /// Sync selected repositories and Projects
    Sync,
    /// Stop automatic GitHub sync (keeps imported tasks and gh credentials)
    Disconnect,
    /// Refresh and list available repositories and Projects
    Sources,
}

pub fn enabled(store: &Store) -> Result<bool> {
    Ok(store.conn.query_row(
        "SELECT enabled FROM github_preferences WHERE id=1",
        [],
        |r| r.get(0),
    )?)
}

trait Api {
    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value>;
}

#[derive(Default)]
struct Gh {
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Api for Gh {
    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            bail!("GitHub sync stopped; local changes retained");
        }
        let read_only = method == "GET"
            || (path == "graphql"
                && body
                    .as_ref()
                    .and_then(|b| b["query"].as_str())
                    .is_some_and(|q| q.trim_start().starts_with("query")));
        let mut command = Process::new("gh");
        command.args(["api", "--hostname", "github.com", "--method", method, path]);
        command
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(std::process::Stdio::null());
        let body_file = body
            .map(|body| -> Result<_> {
                let mut file = tempfile::NamedTempFile::new()?;
                serde_json::to_writer(file.as_file_mut(), &body)?;
                Ok(file)
            })
            .transpose()?;
        if let Some(file) = &body_file {
            command.arg("--input").arg(file.path());
        }
        let output = tempfile::tempfile()?;
        command
            .stdout(output.try_clone()?)
            .stderr(std::process::Stdio::null());
        let mut child = command
            .spawn()
            .context("GitHub CLI is required; install gh, then run `todo github connect`")?;
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if start.elapsed() > std::time::Duration::from_secs(30)
                || (read_only && self.cancel.load(std::sync::atomic::Ordering::Relaxed))
            {
                let _ = child.kill();
                let _ = child.wait();
                bail!("GitHub request stopped or timed out; local changes retained for retry");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        if !status.success() {
            bail!("GitHub request failed for {path}; check `gh auth status --hostname github.com`, permissions, connectivity, or rate limits");
        }
        use std::io::{Seek, SeekFrom};
        let mut output = output;
        output.seek(SeekFrom::Start(0))?;
        serde_json::from_reader(output).context("Invalid GitHub response")
    }
}

#[derive(Deserialize)]
struct Issue {
    number: i64,
    title: String,
    body: Option<String>,
    state: String,
    html_url: String,
    repository_url: String,
    closed_at: Option<String>,
    #[serde(default)]
    assignees: Vec<User>,
}

#[derive(Deserialize)]
struct User {
    id: i64,
    login: String,
}

impl Issue {
    fn assignees(&self) -> Vec<Assignee> {
        let mut values: Vec<_> = self
            .assignees
            .iter()
            .map(|u| Assignee {
                id: u.id,
                username: Some(u.login.clone()),
            })
            .collect();
        values.sort_by_key(|u| u.id);
        values.dedup_by_key(|u| u.id);
        values
    }

    fn repository(&self) -> Result<&str> {
        let repo = self
            .repository_url
            .strip_prefix("https://api.github.com/repos/")
            .context("Unsupported issue host")?;
        let parts: Vec<_> = repo.split('/').collect();
        if parts.len() != 2
            || parts.iter().any(|part| {
                part.is_empty()
                    || !part
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            })
        {
            bail!("Invalid GitHub repository");
        }
        if self.number <= 0
            || self.html_url != format!("https://github.com/{repo}/issues/{}", self.number)
        {
            bail!("Invalid GitHub issue identity");
        }
        Ok(repo)
    }
}

#[derive(Default, Serialize)]
pub struct Report {
    imported: usize,
    pulled: usize,
    pushed: usize,
    pub issues: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

fn import_issue(store: &Store, issue: &Issue) -> Result<(Option<i64>, bool)> {
    let repo = issue.repository()?;
    let existing = store
        .conn
        .query_row(
            "SELECT local_id FROM github_issues WHERE url=?1",
            [&issue.html_url],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok((id, false));
    }
    let tx = rusqlite::Transaction::new_unchecked(
        &store.conn,
        rusqlite::TransactionBehavior::Immediate,
    )?;
    let name = format!("GitHub: {repo}");
    let project = store
        .conn
        .query_row(
            "SELECT id FROM projects WHERE name=?1 COLLATE NOCASE",
            [&name],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .map(Ok)
        .unwrap_or_else(|| store.add_project(&name).map(|p| p.id))?;
    if store.is_clickup_project(Some(project))? {
        bail!("GitHub project name belongs to a ClickUp project; rename it first");
    }
    let task = store.add_task(
        &issue.title,
        issue.body.as_deref().unwrap_or(""),
        Some(project),
        Priority::Normal,
        None,
    )?;
    store.set_done(task.id, issue.state == "closed")?;
    let assignees = serde_json::to_string(&issue.assignees())?;
    store.conn.execute(
        "UPDATE tasks SET assignees=?1 WHERE id=?2",
        params![assignees, task.id],
    )?;
    store.conn.execute("INSERT INTO github_issues(url,repository,number,local_id,base_done,assigned,base_assignees,base_title,base_body) VALUES(?1,?2,?3,?4,?5,0,?6,?7,?8)",
        params![issue.html_url,repo,issue.number,task.id,issue.state=="closed",assignees,task.title,task.notes])?;
    sources::remember(store, "repo", repo, repo, Some(project))?;
    tx.commit()?;
    Ok((Some(task.id), true))
}

fn sync_with(store: &Store, api: &impl Api, discover: bool) -> Result<Report> {
    if !sources::list(store)?.iter().any(|s| s.enabled) {
        return Ok(Report::default());
    }
    let profile = api.call("GET", "user", None)?;
    let login = profile["login"]
        .as_str()
        .context("GitHub profile has no login")?;
    if login.is_empty()
        || !login
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
    {
        bail!("Invalid GitHub login");
    }
    let mut report = Report::default();
    let mut snapshots = std::collections::HashMap::new();
    if discover {
        for source in sources::list(store)?
            .into_iter()
            .filter(|s| s.kind == "repo" && s.enabled)
        {
            for page in 1..=10 {
                if !sources::selected(store, "repo", &source.remote_id)? {
                    break;
                }
                let response = api.call(
                    "GET",
                    &format!(
                        "search/issues?q=is%3Aissue+is%3Aopen+repo%3A{}&per_page=100&page={page}",
                        source.remote_id
                    ),
                    None,
                )?;
                let items = response["items"]
                    .as_array()
                    .context("GitHub search missing issues")?;
                if response["incomplete_results"].as_bool() == Some(true)
                    || response["total_count"].as_u64().unwrap_or(0) > 1000
                {
                    report.issues.push(format!("GitHub search for {} is incomplete or exceeds 1,000 open issues; not all issues were imported",source.remote_id));
                }
                for value in items {
                    let issue: Issue = serde_json::from_value(value.clone())?;
                    if !issue.repository()?.eq_ignore_ascii_case(&source.remote_id) {
                        bail!("GitHub search returned an issue outside the selected repository");
                    }
                    snapshots.insert(issue.html_url.clone(), value.clone());
                    let (_, created) = import_issue(store, &issue)?;
                    report.imported += usize::from(created);
                }
                if items.len() < 100 {
                    break;
                }
            }
        }
        projects::discover(store, api, login, &mut report, false)?;
    }
    let mut stmt = store.conn.prepare(
        "SELECT repository,number,local_id,base_done,base_assignees,base_title,base_body FROM github_issues WHERE local_id IS NOT NULL",
    )?;
    let mappings = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut member_repos = std::collections::HashSet::new();
    for (repo, number, id, base, base_assignees, base_title, base_body) in mappings {
        let result = (|| -> Result<()> {
            if !sources::task_selected(store, id)? {
                return Ok(());
            }
            let local = store.task(id)?;
            let base_assignees: Vec<Assignee> = serde_json::from_str(&base_assignees)?;
            let assignment_changed = local.assignees != base_assignees;
            let title_changed = local.title != base_title;
            let notes_changed = local.notes != base_body;
            let local_changed =
                local.done != base || assignment_changed || title_changed || notes_changed;
            if !discover && !local_changed {
                return Ok(());
            }
            if discover && member_repos.insert(repo.clone()) {
                let members = (|| -> Result<Vec<Assignee>> {
                    let mut members = Vec::new();
                    for page in 1..=100 {
                        let users: Vec<User> = serde_json::from_value(api.call(
                            "GET",
                            &format!("repos/{repo}/assignees?per_page=100&page={page}"),
                            None,
                        )?)?;
                        let count = users.len();
                        members.extend(users.into_iter().map(|u| Assignee {
                            id: u.id,
                            username: Some(u.login),
                        }));
                        if count < 100 {
                            return Ok(members);
                        }
                    }
                    bail!("GitHub repository assignee pagination limit reached");
                })();
                match members {
                    Ok(members) => {
                        store.conn.execute("INSERT INTO github_members(repository,members) VALUES(?1,?2) ON CONFLICT(repository) DO UPDATE SET members=excluded.members", params![repo,serde_json::to_string(&members)?])?;
                    }
                    Err(error) => report
                        .issues
                        .push(format!("Cannot refresh assignees for {repo}: {error:#}")),
                }
            }
            let path = format!("repos/{repo}/issues/{number}");
            let url = format!("https://github.com/{repo}/issues/{number}");
            let remote: Issue = serde_json::from_value(if !local_changed {
                snapshots
                    .remove(&url)
                    .map(Ok)
                    .unwrap_or_else(|| api.call("GET", &path, None))?
            } else {
                api.call("GET", &path, None)?
            })?;
            if remote.repository()? != repo || remote.number != number {
                bail!("Issue moved repositories; local task retained");
            }
            let remote_done = remote.state == "closed";
            let remote_assignees = remote.assignees();
            if assignment_changed
                && remote_assignees != base_assignees
                && remote_assignees != local.assignees
            {
                bail!("Assignee conflict: both copies changed; review GitHub and reopen the assignee picker");
            }
            let mut body = serde_json::Map::new();
            for (field, changed, local_value, remote_value, baseline) in [
                (
                    "title",
                    title_changed,
                    local.title.as_str(),
                    remote.title.as_str(),
                    base_title.as_str(),
                ),
                (
                    "body",
                    notes_changed,
                    local.notes.as_str(),
                    remote.body.as_deref().unwrap_or(""),
                    base_body.as_str(),
                ),
            ] {
                if changed && remote_value != baseline && remote_value != local_value {
                    bail!("GitHub {field} conflict: both copies changed; local edits retained. Make both copies agree before retrying");
                }
                if changed && remote_value != local_value {
                    body.insert(field.into(), json!(local_value));
                }
            }
            if local.done != base && local.done != remote_done {
                body.insert(
                    "state".into(),
                    json!(if local.done { "closed" } else { "open" }),
                );
            }
            if assignment_changed && remote_assignees != local.assignees {
                body.insert(
                    "assignees".into(),
                    json!(local
                        .assignees
                        .iter()
                        .map(|a| a
                            .username
                            .as_deref()
                            .context("GitHub assignee login missing"))
                        .collect::<Result<Vec<_>>>()?),
                );
            }
            let saved = if !body.is_empty() {
                if !sources::task_selected(store, id)? {
                    return Ok(());
                }
                let saved: Issue =
                    serde_json::from_value(api.call("PATCH", &path, Some(Value::Object(body)))?)?;
                if (local.done != base && (saved.state == "closed") != local.done)
                    || (assignment_changed && saved.assignees() != local.assignees)
                    || (title_changed && saved.title != local.title)
                    || (notes_changed && saved.body.as_deref().unwrap_or("") != local.notes)
                {
                    bail!(
                        "GitHub did not retain the requested changes; check repository permissions"
                    );
                }
                report.pushed += 1;
                saved
            } else {
                remote
            };
            let done = saved.state == "closed";
            let tx = rusqlite::Transaction::new_unchecked(
                &store.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let current = store.task(id)?;
            let saved_assignees = saved.assignees();
            let saved_notes = saved.body.as_deref().unwrap_or("");
            let pull_title = current.title == local.title && current.title != saved.title;
            let pull_notes = current.notes == local.notes && current.notes != saved_notes;
            if pull_title || pull_notes {
                store.conn.execute(
                    "UPDATE tasks SET title=CASE WHEN ?1 THEN ?2 ELSE title END,
                     notes=CASE WHEN ?3 THEN ?4 ELSE notes END,
                     updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?5",
                    params![pull_title, saved.title, pull_notes, saved_notes, id],
                )?;
                report.pulled += 1;
            }
            if current.assignees == local.assignees && current.assignees != saved_assignees {
                store.conn.execute("UPDATE tasks SET assignees=?1,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?2", params![serde_json::to_string(&saved_assignees)?,id])?;
            }
            if current.done == local.done
                && current.in_progress == local.in_progress
                && done != local.done
            {
                store.set_done(id, done)?;
                report.pulled += 1;
            }
            if current.done == local.done && done {
                store.conn.execute(
                    "UPDATE tasks SET completed_at=?1 WHERE id=?2",
                    params![saved.closed_at, id],
                )?;
            }
            store.conn.execute(
                "UPDATE github_issues SET base_done=?1,assigned=?2,base_assignees=?4,base_title=?5,base_body=?6 WHERE local_id=?3",
                params![
                    done,
                    saved
                        .assignees
                        .iter()
                        .any(|u| u.login.eq_ignore_ascii_case(login)),
                    id,
                    serde_json::to_string(&saved_assignees)?,
                    saved.title,
                    saved_notes
                ],
            )?;
            tx.commit()?;
            Ok(())
        })();
        if let Err(error) = result {
            report
                .issues
                .push(format!("Task {id} ({repo}#{number}): {error:#}"));
        }
    }
    Ok(report)
}

fn sync_api(database: &Path, discover: bool, api: &impl Api) -> Result<Report> {
    let store = Store::open(database)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(database.canonicalize()?.with_extension("github.lock"))?;
    lock.try_lock_exclusive()
        .context("Another GitHub sync is running; retry shortly")?;
    sync_with(&store, api, discover)
}

pub fn sync(database: &Path, discover: bool) -> Result<Report> {
    sync_api(database, discover, &Gh::default())
}

pub fn run(command: Command, database: &Path, json: bool) -> Result<()> {
    let store = Store::open(database)?;
    match command {
        Command::Sources => {
            let report = sources::refresh(&store, &Gh::default())?;
            let sources = sources::list(&store)?;
            return crate::output(
                json,
                &json!({"sources":sources,"issues":report.issues}),
                || {
                    for source in &sources {
                        println!(
                            "[{}] {}: {}",
                            if source.enabled { "x" } else { " " },
                            source.kind,
                            source.label
                        );
                    }
                    for issue in &report.issues {
                        eprintln!("{issue}");
                    }
                    println!("Open todo and press ? → GitHub sources to select what to sync.");
                },
            );
        }
        Command::Disconnect => {
            store
                .conn
                .execute("UPDATE github_preferences SET enabled=0", [])?;
            return crate::output(json, &json!({"enabled":false}), || {
                println!("GitHub sync disabled; gh credentials and tasks retained.")
            });
        }
        Command::Connect => {
            use std::io::IsTerminal;
            if json || !std::io::stdin().is_terminal() {
                bail!("Connect requires a terminal; run `gh auth login --hostname github.com` then `todo github sync`");
            }
            if !Process::new("gh")
                .args([
                    "auth",
                    "login",
                    "--hostname",
                    "github.com",
                    "--web",
                    "--scopes",
                    "project",
                ])
                .status()
                .context("Install GitHub CLI (gh) first")?
                .success()
            {
                bail!("GitHub login failed");
            }
        }
        Command::Sync => {}
    }
    Gh::default().call("GET", "user", None)?;
    store
        .conn
        .execute("UPDATE github_preferences SET enabled=1", [])?;
    let report = if matches!(command, Command::Connect) {
        sources::refresh(&store, &Gh::default())?
    } else {
        sync(database, true)?
    };
    crate::output(json, &report, || {
        println!(
            "GitHub: imported {} | pulled {} | pushed {}\n{}",
            report.imported,
            report.pulled,
            report.pushed,
            report.issues.join("\n")
        )
    })?;
    if !report.issues.is_empty() {
        bail!("Some GitHub issues could not be synced; local data retained");
    }
    Ok(())
}

pub enum Request {
    Sync(bool),
    Sources,
    Buckets(u64, i64, crate::store::Scope),
    MoveBucket(projects::Bucket),
}

pub struct Worker {
    pub requests: std::sync::mpsc::Sender<Request>,
    pub results: std::sync::mpsc::Receiver<Result<Report>>,
    pub buckets: std::sync::mpsc::Receiver<(u64, Result<Vec<projects::Bucket>>)>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    pub fn stop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self.requests.send(Request::Sync(false));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    pub fn start(database: &Path) -> Self {
        let (requests, receive) = std::sync::mpsc::channel::<Request>();
        let (send, results) = std::sync::mpsc::channel();
        let (bucket_send, buckets) = std::sync::mpsc::channel();
        let path = database.to_path_buf();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = cancel.clone();
        let thread = std::thread::spawn(move || {
            while let Ok(request) = receive.recv() {
                let mut discover = matches!(request, Request::Sync(true));
                let mut catalog = matches!(request, Request::Sources);
                let mut sync = matches!(request, Request::Sync(_));
                let mut jobs = vec![request];
                std::thread::sleep(std::time::Duration::from_millis(200));
                while let Ok(next) = receive.try_recv() {
                    discover |= matches!(next, Request::Sync(true));
                    catalog |= matches!(next, Request::Sources);
                    sync |= matches!(next, Request::Sync(_));
                    jobs.push(next);
                }
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                for job in jobs {
                    match job {
                        Request::Buckets(generation, task, scope) => {
                            let result = Store::open(&path).and_then(|store| {
                                projects::buckets(
                                    &store,
                                    &Gh {
                                        cancel: stop.clone(),
                                    },
                                    task,
                                    scope,
                                )
                            });
                            let _ = bucket_send.send((generation, result));
                        }
                        Request::MoveBucket(bucket) => {
                            let result = Store::open(&path).and_then(|store| {
                                projects::move_bucket(
                                    &store,
                                    &Gh {
                                        cancel: stop.clone(),
                                    },
                                    &bucket,
                                )?;
                                Ok(Report {
                                    pushed: 1,
                                    message: Some(format!(
                                        "GitHub bucket updated: {}",
                                        bucket.label
                                    )),
                                    ..Default::default()
                                })
                            });
                            let _ = send.send(result);
                        }
                        _ => {}
                    }
                }
                if !sync && !catalog {
                    continue;
                }
                let result = (|| {
                    let store = Store::open(&path)?;
                    if !catalog && !enabled(&store)? {
                        return Ok(Report::default());
                    }
                    let api = Gh {
                        cancel: stop.clone(),
                    };
                    if catalog {
                        let report = sources::refresh(&store, &api)?;
                        if !discover {
                            return Ok(report);
                        }
                        let mut synced = sync_api(&path, true, &api)?;
                        synced.issues.extend(report.issues);
                        return Ok(synced);
                    }
                    sync_api(
                        &path,
                        discover,
                        &Gh {
                            cancel: stop.clone(),
                        },
                    )
                })();
                if send.send(result).is_err() {
                    break;
                }
            }
        });
        Self {
            requests,
            results,
            buckets,
            cancel,
            thread: Some(thread),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Mock {
        issues: RefCell<Vec<Value>>,
        writes: RefCell<Vec<Value>>,
        fail: bool,
    }

    impl Api for Mock {
        fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
            if self.fail {
                bail!("offline");
            }
            if path == "user" {
                return Ok(json!({"login":"sam"}));
            }
            if path.starts_with("user/repos?") {
                return Ok(json!([{"full_name":"acme/project"}]));
            }
            if path == "graphql" {
                let query = body.as_ref().unwrap()["query"].as_str().unwrap();
                return Ok(if query.contains("organizations(") {
                    json!({"data":{"viewer":{"organizations":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}})
                } else {
                    json!({"data":{"user":{"projectsV2":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}})
                });
            }
            if path.starts_with("repos/acme/project/assignees?") {
                return Ok(
                    json!([{"id":7,"login":"sam"},{"id":8,"login":"other"},{"id":9,"login":"new-person"}]),
                );
            }
            if path.starts_with("search/issues") {
                assert!(path.contains("repo%3Aacme/project"));
                return Ok(
                    json!({"items":self.issues.borrow().iter().filter(|v| v["state"]=="open").cloned().collect::<Vec<_>>(),"total_count":self.issues.borrow().len(),"incomplete_results":false}),
                );
            }
            let number: i64 = path.rsplit('/').next().unwrap().parse()?;
            let mut issues = self.issues.borrow_mut();
            let issue = issues
                .iter_mut()
                .find(|i| i["number"] == number)
                .context("Not found")?;
            if method == "PATCH" {
                let body = body.unwrap();
                for field in ["title", "body"] {
                    if let Some(value) = body.get(field) {
                        issue[field] = value.clone();
                    }
                }
                if let Some(state) = body.get("state") {
                    issue["state"] = state.clone();
                    issue["closed_at"] = if state == "closed" {
                        json!("2026-10-29T13:00:00Z")
                    } else {
                        Value::Null
                    };
                }
                if let Some(values) = body["assignees"].as_array() {
                    issue["assignees"] = json!(values.iter().map(|name| json!({"id":match name.as_str().unwrap() { "sam"=>7,"other"=>8,_=>9 },"login":name})).collect::<Vec<_>>());
                }
                self.writes.borrow_mut().push(body);
            }
            Ok(issue.clone())
        }
    }

    fn issue(number: i64) -> Value {
        json!({"number":number,"title":format!("Issue {number}"),"body":"Issue notes",
                "state":"open","html_url":format!("https://github.com/acme/project/issues/{number}"),
                "repository_url":"https://api.github.com/repos/acme/project","closed_at":null,
                "assignees":[{"id":7,"login":"sam"},{"id":8,"login":"other"}]})
    }

    #[test]
    fn selected_repo_issues_import_once_and_close_reopen_in_both_directions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        sources::remember(&store, "repo", "acme/project", "acme/project", None).unwrap();
        sources::set_enabled(&store, "repo", "acme/project", true).unwrap();
        let api = Mock {
            issues: RefCell::new(vec![issue(1), issue(2)]),
            writes: RefCell::new(vec![]),
            fail: false,
        };
        assert_eq!(sync_with(&store, &api, true).unwrap().imported, 2);
        assert_eq!(sync_with(&store, &api, true).unwrap().imported, 0);
        let task = store
            .tasks(crate::store::Scope::All, crate::store::Status::All, "")
            .unwrap()
            .remove(0);
        let number = task
            .github_url
            .as_ref()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap()
            .parse::<i64>()
            .unwrap();
        assert!(task.github_assigned);
        assert_eq!(task.notes, "Issue notes");
        store.set_done(task.id, true).unwrap();
        assert_eq!(sync_with(&store, &api, false).unwrap().pushed, 1);
        assert!(store.task(task.id).unwrap().completed_at.is_some());
        store.set_done(task.id, false).unwrap();
        assert_eq!(sync_with(&store, &api, false).unwrap().pushed, 1);
        assert!(store.task(task.id).unwrap().completed_at.is_none());
        {
            let mut issues = api.issues.borrow_mut();
            let issue = issues.iter_mut().find(|i| i["number"] == number).unwrap();
            issue["state"] = json!("closed");
            issue["closed_at"] = json!("2026-10-29T13:00:00Z");
            issue["assignees"] = json!([{"id":8,"login":"other"}]);
        }
        assert_eq!(sync_with(&store, &api, true).unwrap().pulled, 1);
        let closed = store.task(task.id).unwrap();
        assert!(closed.done);
        assert!(!closed.github_assigned);
        assert_eq!(closed.completed_at.as_deref(), Some("2026-10-29T13:00:00Z"));
        api.issues
            .borrow_mut()
            .iter_mut()
            .find(|i| i["number"] == number)
            .unwrap()["state"] = json!("open");
        assert_eq!(sync_with(&store, &api, true).unwrap().pulled, 1);
        assert!(!store.task(task.id).unwrap().done);
        store.delete_task(task.id).unwrap();
        assert_eq!(sync_with(&store, &api, true).unwrap().imported, 0);
        assert_eq!(api.writes.borrow().len(), 2);
    }

    #[test]
    fn github_assignments_round_trip_clear_and_preserve_unrelated_remote_changes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        sources::remember(&store, "repo", "acme/project", "acme/project", None).unwrap();
        sources::set_enabled(&store, "repo", "acme/project", true).unwrap();
        let api = Mock {
            issues: RefCell::new(vec![issue(1)]),
            writes: RefCell::new(vec![]),
            fail: false,
        };
        sync_with(&store, &api, true).unwrap();
        let task = store
            .tasks(crate::store::Scope::All, crate::store::Status::All, "")
            .unwrap()
            .remove(0);
        assert_eq!(store.github_members(task.id).unwrap().len(), 3);
        store.assign(task.id, &[9, 7]).unwrap();
        assert_eq!(sync_with(&store, &api, false).unwrap().pushed, 1);
        assert_eq!(
            api.writes.borrow()[0],
            json!({"assignees":["sam","new-person"]})
        );
        api.issues.borrow_mut()[0]["assignees"] = json!([{"id":8,"login":"other"}]);
        sync_with(&store, &api, true).unwrap();
        assert_eq!(store.task(task.id).unwrap().assignees[0].id, 8);
        assert!(!store.task(task.id).unwrap().github_assigned);
        store.assign(task.id, &[]).unwrap();
        assert_eq!(sync_with(&store, &api, false).unwrap().pushed, 1);
        assert_eq!(
            api.writes.borrow().last().unwrap(),
            &json!({"assignees":[]})
        );
        store.assign(task.id, &[7]).unwrap();
        api.issues.borrow_mut()[0]["assignees"] = json!([{"id":9,"login":"new-person"}]);
        let report = sync_with(&store, &api, false).unwrap();
        assert!(report.issues[0].contains("Assignee conflict"));
        assert_eq!(api.writes.borrow().len(), 2);
    }

    #[test]
    fn offline_sync_and_repository_collisions_preserve_local_data() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        sources::remember(&store, "repo", "acme/project", "acme/project", None).unwrap();
        sources::set_enabled(&store, "repo", "acme/project", true).unwrap();
        let local = store
            .add_task("Keep", "", None, Priority::High, None)
            .unwrap();
        let api = Mock {
            issues: RefCell::new(vec![issue(1)]),
            writes: RefCell::new(vec![]),
            fail: true,
        };
        assert!(sync_with(&store, &api, true).is_err());
        assert_eq!(store.task(local.id).unwrap().title, "Keep");
        let project = store.add_project("GitHub: acme/project").unwrap();
        store.conn.execute("INSERT INTO clickup_projects(project_id,list_id,open_status,working_status,done_status,timezone) VALUES (?1,'123','to do','in progress','done','UTC')",[project.id]).unwrap();
        let api = Mock { fail: false, ..api };
        assert!(sync_with(&store, &api, true).is_err());
        assert_eq!(
            store
                .tasks(crate::store::Scope::All, crate::store::Status::All, "")
                .unwrap()
                .len(),
            1
        );
        assert!(api.writes.borrow().is_empty());
    }

    #[test]
    fn issue_titles_and_notes_sync_both_ways_merge_and_detect_conflicts() {
        use crate::store::TaskEdit;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        sources::remember(&store, "repo", "acme/project", "acme/project", None).unwrap();
        sources::set_enabled(&store, "repo", "acme/project", true).unwrap();
        let api = Mock {
            issues: RefCell::new(vec![issue(1)]),
            writes: RefCell::new(vec![]),
            fail: false,
        };
        sync_with(&store, &api, true).unwrap();
        let id = store
            .tasks(crate::store::Scope::All, crate::store::Status::All, "")
            .unwrap()[0]
            .id;
        store
            .edit_task(
                id,
                TaskEdit {
                    title: Some("New title".into()),
                    notes: Some("Description\n\nMore details".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(sync_with(&store, &api, false).unwrap().pushed, 1);
        assert_eq!(
            api.writes.borrow()[0],
            json!({"title":"New title","body":"Description\n\nMore details"})
        );
        api.issues.borrow_mut()[0]["title"] = json!("Remote title");
        api.issues.borrow_mut()[0]["body"] = Value::Null;
        sync_with(&store, &api, true).unwrap();
        let task = store.task(id).unwrap();
        assert_eq!(task.title, "Remote title");
        assert!(task.notes.is_empty());
        store
            .edit_task(
                id,
                TaskEdit {
                    title: Some("Local title".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        api.issues.borrow_mut()[0]["body"] = json!("Remote notes");
        let report = sync_with(&store, &api, false).unwrap();
        assert!(report.issues.is_empty());
        assert_eq!(
            api.writes.borrow().last().unwrap(),
            &json!({"title":"Local title"})
        );
        assert_eq!(store.task(id).unwrap().notes, "Remote notes");
        store
            .edit_task(
                id,
                TaskEdit {
                    notes: Some(String::new()),
                    ..Default::default()
                },
            )
            .unwrap();
        sync_with(&store, &api, false).unwrap();
        assert_eq!(api.writes.borrow().last().unwrap(), &json!({"body":""}));
        for (field, edit) in [
            (
                "title",
                TaskEdit {
                    title: Some("Conflict title".into()),
                    ..Default::default()
                },
            ),
            (
                "body",
                TaskEdit {
                    notes: Some("Conflict notes".into()),
                    ..Default::default()
                },
            ),
        ] {
            store.edit_task(id, edit).unwrap();
            api.issues.borrow_mut()[0][field] = json!("Different remote edit");
            let writes = api.writes.borrow().len();
            let report = sync_with(&store, &api, false).unwrap();
            assert!(report
                .issues
                .iter()
                .any(|e| e.contains(&format!("{field} conflict"))));
            assert_eq!(api.writes.borrow().len(), writes);
            assert_eq!(api.issues.borrow()[0][field], "Different remote edit");
            let task = store.task(id).unwrap();
            api.issues.borrow_mut()[0][field] = json!(if field == "title" {
                task.title
            } else {
                task.notes
            });
            assert!(sync_with(&store, &api, false).unwrap().issues.is_empty());
        }
    }

    #[test]
    fn title_edits_during_push_remain_pending_and_offline_edits_retry() {
        use crate::store::TaskEdit;
        struct Concurrent<'a> {
            api: &'a Mock,
            store: &'a Store,
            id: i64,
        }
        impl Api for Concurrent<'_> {
            fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
                let result = self.api.call(method, path, body)?;
                if method == "PATCH" {
                    self.store.edit_task(
                        self.id,
                        TaskEdit {
                            title: Some("Newer title".into()),
                            notes: Some("Newer notes".into()),
                            ..Default::default()
                        },
                    )?;
                }
                Ok(result)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        sources::remember(&store, "repo", "acme/project", "acme/project", None).unwrap();
        sources::set_enabled(&store, "repo", "acme/project", true).unwrap();
        let api = Mock {
            issues: RefCell::new(vec![issue(1)]),
            writes: RefCell::new(vec![]),
            fail: false,
        };
        sync_with(&store, &api, true).unwrap();
        let id = store
            .tasks(crate::store::Scope::All, crate::store::Status::All, "")
            .unwrap()[0]
            .id;
        store
            .edit_task(
                id,
                TaskEdit {
                    title: Some("Sent title".into()),
                    notes: Some("Sent notes".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let api = Mock { fail: true, ..api };
        assert!(sync_with(&store, &api, false).is_err());
        assert_eq!(store.task(id).unwrap().title, "Sent title");
        let api = Mock { fail: false, ..api };
        sync_with(
            &store,
            &Concurrent {
                api: &api,
                store: &store,
                id,
            },
            false,
        )
        .unwrap();
        assert_eq!(store.task(id).unwrap().title, "Newer title");
        assert_eq!(store.task(id).unwrap().notes, "Newer notes");
        assert_eq!(api.issues.borrow()[0]["title"], "Sent title");
        assert_eq!(sync_with(&store, &api, false).unwrap().pushed, 1);
        assert_eq!(api.issues.borrow()[0]["title"], "Newer title");
        assert_eq!(api.issues.borrow()[0]["body"], "Newer notes");
        assert_eq!(sync_with(&store, &api, false).unwrap().pushed, 0);
    }

    #[test]
    fn catalog_is_opt_in_and_removing_a_source_stops_network_and_preserves_cached_tasks() {
        use crate::store::{Scope, Status};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = Store::open(&path).unwrap();
        let api = Mock {
            issues: RefCell::new(vec![issue(1)]),
            writes: RefCell::new(vec![]),
            fail: false,
        };
        assert_eq!(sources::refresh(&store, &api).unwrap().imported, 0);
        assert!(store.tasks(Scope::All, Status::All, "").unwrap().is_empty());
        assert!(!sources::list(&store).unwrap()[0].enabled);
        sources::set_enabled(&store, "repo", "acme/project", true).unwrap();
        sources::refresh(&store, &api).unwrap();
        assert!(sources::list(&store).unwrap()[0].enabled);
        sync_with(&store, &api, true).unwrap();
        let task = store.tasks(Scope::All, Status::All, "").unwrap().remove(0);
        store.set_done(task.id, true).unwrap();
        sources::set_enabled(&store, "repo", "acme/project", false).unwrap();
        assert!(store.tasks(Scope::All, Status::All, "").unwrap().is_empty());
        assert!(store.visible_projects().unwrap().is_empty());
        let api = Mock { fail: true, ..api };
        assert_eq!(sync_with(&store, &api, true).unwrap().pushed, 0);
        assert_eq!(sync_with(&store, &api, false).unwrap().pushed, 0);
        assert!(api.writes.borrow().is_empty());
        assert!(store.task(task.id).unwrap().done);
        drop(store);
        let store = Store::open(&path).unwrap();
        assert!(!sources::list(&store).unwrap()[0].enabled);
        sources::set_enabled(&store, "repo", "acme/project", true).unwrap();
        let api = Mock { fail: false, ..api };
        assert_eq!(sync_with(&store, &api, false).unwrap().pushed, 1);
        assert_eq!(
            store.tasks(Scope::All, Status::All, "").unwrap()[0].id,
            task.id
        );
    }

    #[test]
    fn migrating_existing_github_cache_starts_with_no_sources_selected() {
        use crate::store::{Scope, Status};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = Store::open(&path).unwrap();
        let remote: Issue = serde_json::from_value(issue(1)).unwrap();
        let (id, _) = import_issue(&store, &remote).unwrap();
        let local = store
            .add_task("Keep local", "", None, Priority::Normal, None)
            .unwrap();
        store
            .conn
            .execute_batch("DROP TABLE github_sources; DROP INDEX github_project_items_local; PRAGMA user_version=8;")
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.tasks(Scope::All, Status::All, "").unwrap()[0].id,
            local.id
        );
        assert!(store.visible_projects().unwrap().is_empty());
        assert_eq!(store.task(id.unwrap()).unwrap().title, remote.title);
        let source = sources::list(&store).unwrap().remove(0);
        assert!(!source.enabled);
        sources::set_enabled(&store, &source.kind, &source.remote_id, true).unwrap();
        assert_eq!(store.tasks(Scope::All, Status::All, "").unwrap().len(), 2);
        assert_eq!(store.visible_projects().unwrap().len(), 1);
    }
}
