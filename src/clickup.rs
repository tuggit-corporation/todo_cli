use crate::store::{Assignee, Priority, Scope, Status, Store, Task, TaskEdit};
use anyhow::{bail, Context, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use clap::{Subcommand, ValueEnum};
use fs2::FileExt;
use reqwest::{
    blocking::Client,
    header::{HeaderMap, HeaderValue, AUTHORIZATION},
    Method,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{collections::HashSet, fs::OpenOptions, io::IsTerminal, path::Path, time::Duration};

const TOKEN_URL: &str = "https://app.clickup.com/settings/apps";

#[derive(Subcommand)]
pub enum Command {
    /// Open ClickUp token settings, validate a pasted token, and save it in the OS credential store
    Connect {
        #[arg(long)]
        no_browser: bool,
    },
    /// Remove the saved token (does not revoke it in ClickUp)
    Disconnect,
    /// Show a ClickUp List's name and available statuses before linking
    List { list_id: String },
    /// Link a local project to a ClickUp List; sync sends all its tasks to that List
    Link {
        project: i64,
        list_id: String,
        /// Your ClickUp profile timezone, e.g. Europe/Stockholm (used for date-only deadlines)
        #[arg(long)]
        timezone: Tz,
        #[arg(long)]
        open_status: String,
        #[arg(long)]
        working_status: String,
        #[arg(long)]
        done_status: String,
    },
    /// Show local project/List links; no network access
    Links,
    /// Discover accessible workspace/Space/Folder Lists (read-only)
    Discover,
    /// Choose accessible Lists and import each into its own linked local project
    Import,
    /// Configure automatic synchronization and discovery
    Auto {
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
        #[arg(long)]
        enable: bool,
        #[arg(long)]
        timezone: Option<Tz>,
    },
    /// Show assignable List members for a linked project
    Members { project: i64 },
    /// Sync one linked project in both directions (never deletes tasks)
    Sync {
        project: i64,
        /// Resolve only this conflicting task using local values
        #[arg(long, conflicts_with = "prefer_remote")]
        prefer_local: Option<i64>,
        /// Resolve only this conflicting task using ClickUp values
        #[arg(long)]
        prefer_remote: Option<i64>,
    },
    /// Recover a create whose result was uncertain; inspect ClickUp first, then supply its task ID
    Recover { task: i64, remote_id: String },
    /// Clear an uncertain create ONLY after verifying that no task was created in ClickUp
    RetryCreate {
        task: i64,
        #[arg(long, required = true)]
        confirm_missing: bool,
    },
}

#[derive(Deserialize)]
struct Named {
    #[serde(deserialize_with = "string_id")]
    id: String,
    name: String,
}

fn string_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    match Value::deserialize(deserializer)? {
        Value::String(id) => Ok(id),
        Value::Number(id) => Ok(id.to_string()),
        _ => Err(serde::de::Error::custom("Expected a string or numeric ID")),
    }
}

#[derive(Serialize)]
struct DiscoveredList {
    id: String,
    name: String,
    workspace: String,
    space: String,
    folder: Option<String>,
}

#[derive(Default, Serialize)]
struct Discovery {
    lists: Vec<DiscoveredList>,
    warnings: Vec<String>,
}

fn collection(api: &Api, path: &str, key: &str) -> Result<Vec<Named>> {
    let response = api.get(path)?;
    let items: Vec<Named> = serde_json::from_value(
        response
            .get(key)
            .with_context(|| format!("ClickUp response is missing {key}"))?
            .clone(),
    )?;
    for item in &items {
        identifier(&item.id)?;
    }
    Ok(items)
}

fn discover(api: &Api) -> Result<Discovery> {
    let mut discovery = Discovery::default();
    let mut seen = HashSet::new();
    for workspace in collection(api, "/team", "teams")? {
        let spaces = match collection(
            api,
            &format!("/team/{}/space?archived=false", workspace.id),
            "spaces",
        ) {
            Ok(spaces) => spaces,
            Err(error) => {
                discovery
                    .warnings
                    .push(format!("Workspace {}: {error:#}", workspace.name));
                continue;
            }
        };
        for space in spaces {
            let mut locations = vec![(format!("/space/{}/list?archived=false", space.id), None)];
            match collection(
                api,
                &format!("/space/{}/folder?archived=false", space.id),
                "folders",
            ) {
                Ok(folders) => locations.extend(folders.into_iter().map(|f| {
                    (
                        format!("/folder/{}/list?archived=false", f.id),
                        Some(f.name),
                    )
                })),
                Err(error) => discovery.warnings.push(format!(
                    "{}/{} folders: {error:#}",
                    workspace.name, space.name
                )),
            }
            for (path, folder) in locations {
                match collection(api, &path, "lists") {
                    Ok(lists) => {
                        for list in lists {
                            if seen.insert(list.id.clone()) {
                                discovery.lists.push(DiscoveredList {
                                    id: list.id,
                                    name: list.name,
                                    workspace: workspace.name.clone(),
                                    space: space.name.clone(),
                                    folder: folder.clone(),
                                });
                            }
                        }
                    }
                    Err(error) => discovery.warnings.push(format!(
                        "{}/{} Lists: {error:#}",
                        workspace.name, space.name
                    )),
                }
            }
        }
    }
    Ok(discovery)
}

fn prompt(label: &str) -> Result<String> {
    use std::io::Write;
    eprint!("{label}");
    std::io::stderr().flush()?;
    let mut value = String::new();
    if std::io::stdin().read_line(&mut value)? == 0 {
        bail!("Input closed; import cancelled");
    }
    Ok(value.trim().to_owned())
}

fn selections(input: &str, count: usize) -> Result<Vec<usize>> {
    let mut selected = Vec::new();
    for part in input.split(',').map(str::trim) {
        let index = part
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0 && *n <= count)
            .context("Choose comma-separated List numbers, or press Enter to cancel")?
            - 1;
        if !selected.contains(&index) {
            selected.push(index);
        }
    }
    Ok(selected)
}

fn choose_status(
    metadata: &Value,
    label: &str,
    completed: bool,
    preferred: &[&str],
) -> Result<String> {
    let candidates: Vec<_> = metadata["statuses"]
        .as_array()
        .context("List has no statuses")?
        .iter()
        .filter(|s| matches!(s["type"].as_str(), Some("done" | "closed")) == completed)
        .filter_map(|s| s["status"].as_str())
        .collect();
    if candidates.is_empty() {
        bail!("List has no compatible {label} status");
    }
    let default = preferred
        .iter()
        .find_map(|name| candidates.iter().position(|c| c.eq_ignore_ascii_case(name)));
    eprintln!("{label} status:");
    for (index, name) in candidates.iter().enumerate() {
        eprintln!("  {}. {}", index + 1, crate::safe(name));
    }
    loop {
        let answer = prompt(&format!(
            "Choose {}{}: ",
            label,
            default
                .map(|i| format!(" [Enter = {}]", i + 1))
                .unwrap_or_default()
        ))?;
        let selected = if answer.is_empty() {
            default
        } else {
            answer
                .parse::<usize>()
                .ok()
                .and_then(|i| i.checked_sub(1))
                .filter(|i| *i < candidates.len())
        };
        if let Some(index) = selected {
            return Ok(candidates[index].to_owned());
        }
        eprintln!("Choose one of the numbers above.");
    }
}

fn import_link(store: &Store, api: &Api, list: &DiscoveredList, timezone: Tz) -> Result<Link> {
    if let Some(link) = links(store)?.into_iter().find(|l| l.list_id == list.id) {
        store_location(store, list)?;
        eprintln!(
            "Already linked to project {}; syncing that project.",
            link.project_id
        );
        return Ok(link);
    }
    let metadata = api.get(&format!("/list/{}", list.id))?;
    let open_status = choose_status(
        &metadata,
        "Not started",
        false,
        &["to do", "open", "backlog"],
    )?;
    let working_status = choose_status(
        &metadata,
        "In progress",
        false,
        &["in progress", "doing", "working"],
    )?;
    let done_status = choose_status(
        &metadata,
        "Completed",
        true,
        &["closed", "complete", "done"],
    )?;
    let mut link = Link {
        project_id: 0,
        list_id: list.id.clone(),
        open_status,
        working_status,
        done_status,
        timezone,
    };
    validate_statuses(&metadata, &link)?;
    // Validate all task pages before persisting a link, so access failures don't
    // leave an apparently imported project behind.
    api.tasks(&list.id)?;
    atomic(store, || {
        let existing = store.projects()?;
        let base = format!("ClickUp: {}", list.name);
        let mut name = base.clone();
        let mut suffix = 1;
        while existing.iter().any(|p| p.name.eq_ignore_ascii_case(&name)) {
            name = format!("{base} ({suffix})");
            suffix += 1;
        }
        let project = store.add_project(&name)?;
        link.project_id = project.id;
        store.conn.execute("INSERT INTO clickup_projects(project_id,list_id,open_status,working_status,done_status,timezone) VALUES (?1,?2,?3,?4,?5,?6)",
            params![project.id,link.list_id,link.open_status,link.working_status,link.done_status,link.timezone.to_string()])?;
        store_location(store, list)?;
        Ok(())
    })?;
    Ok(link)
}

fn interactive_import(store: &Store, api: &Api) -> Result<()> {
    eprintln!("Discovering accessible ClickUp Lists...");
    let discovery = discover(api)?;
    for warning in &discovery.warnings {
        eprintln!("Warning: {}", crate::safe(warning));
    }
    if discovery.lists.is_empty() {
        bail!("No accessible, non-archived Lists found. Guest-shared Lists may require manual `clickup link`");
    }
    let linked: HashSet<_> = links(store)?.into_iter().map(|l| l.list_id).collect();
    for (index, list) in discovery.lists.iter().enumerate() {
        eprintln!(
            "{:>3}. {} / {} / {}{}{}",
            index + 1,
            crate::safe(&list.workspace),
            crate::safe(&list.space),
            list.folder
                .as_ref()
                .map(|f| format!("{} / ", crate::safe(f)))
                .unwrap_or_default(),
            crate::safe(&list.name),
            if linked.contains(&list.id) {
                " [linked]"
            } else {
                ""
            }
        );
    }
    let chosen = loop {
        let answer =
            prompt("Import which Lists? Comma-separated numbers (e.g. 1,3), Enter cancels: ")?;
        if answer.is_empty() {
            return Ok(());
        }
        match selections(&answer, discovery.lists.len()) {
            Ok(chosen) => break chosen,
            Err(error) => eprintln!("{error}"),
        }
    };
    let profile = api.get("/user")?;
    let suggested = profile["user"]["timezone"]
        .as_str()
        .and_then(|v| v.parse::<Tz>().ok());
    let timezone = loop {
        let answer = prompt(&format!(
            "ClickUp profile timezone (e.g. Europe/Stockholm){}: ",
            suggested
                .map(|t| format!(" [Enter = {t}]"))
                .unwrap_or_default()
        ))?;
        if answer.is_empty() {
            if let Some(tz) = suggested {
                break tz;
            }
        } else if let Ok(tz) = answer.parse::<Tz>() {
            break tz;
        }
        eprintln!("Enter a valid IANA timezone matching your ClickUp profile.");
    };
    eprintln!("Each List gets a separate linked project. Future explicit syncs exchange edits in both directions; deletions never propagate.");
    if !matches!(
        prompt("Import selected Lists now? [y/N] ")?
            .to_ascii_lowercase()
            .as_str(),
        "y" | "yes"
    ) {
        return Ok(());
    }
    let mut failures = 0;
    for index in chosen {
        let list = &discovery.lists[index];
        eprintln!("\n{}", crate::safe(&list.name));
        let result = (|| -> Result<()> {
            let link = import_link(store, api, list, timezone)?;
            let report = sync(store, api, &link, None)?;
            println!(
                "Project {}: imported {}, created {}, pulled {}, pushed {}",
                link.project_id, report.imported, report.created, report.pulled, report.pushed
            );
            for issue in &report.issues {
                eprintln!("{}", crate::safe(&issue.message));
            }
            if !report.issues.is_empty() {
                bail!(
                    "Some tasks could not be synced; run `todo clickup sync {}` to retry",
                    link.project_id
                );
            }
            Ok(())
        })();
        if let Err(error) = result {
            failures += 1;
            eprintln!("{}: {error:#}", crate::safe(&list.name));
        }
    }
    if failures > 0 {
        bail!("{failures} selected List(s) need attention; successful imports were kept");
    }
    println!("ClickUp import complete. Open todo to see your new projects.");
    Ok(())
}

fn identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        bail!("Invalid ClickUp ID; use the ID, not a URL");
    }
    Ok(())
}

fn credential() -> Result<keyring::Entry> {
    keyring::Entry::new("todo.clickup", "personal-api-token")
        .context("Cannot access OS credential store; use CLICKUP_API_TOKEN instead")
}

fn token() -> Result<String> {
    let token = match std::env::var("CLICKUP_API_TOKEN") {
        Ok(token) => token,
        Err(std::env::VarError::NotPresent) => credential()?.get_password().context(
            "No ClickUp token available. Run `todo clickup connect` or set CLICKUP_API_TOKEN",
        )?,
        Err(_) => bail!("CLICKUP_API_TOKEN must contain valid text"),
    };
    if token.trim().is_empty() {
        bail!("ClickUp token is empty");
    }
    Ok(token.trim().to_owned())
}

struct Api {
    client: Client,
    base: String,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    rate_limited: std::cell::Cell<bool>,
}

impl Api {
    fn new(token: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let mut authorization = HeaderValue::from_str(token)
            .map_err(|_| anyhow::anyhow!("Invalid ClickUp token format"))?;
        authorization.set_sensitive(true);
        headers.insert(AUTHORIZATION, authorization);
        Ok(Self {
            client: Client::builder()
                .default_headers(headers)
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(25))
                .user_agent(concat!("todo/", env!("CARGO_PKG_VERSION")))
                .build()?,
            base: "https://api.clickup.com/api/v2".into(),
            cancel: None,
            rate_limited: std::cell::Cell::new(false),
        })
    }

    fn request(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        if self.rate_limited.get() {
            bail!("ClickUp returned HTTP 429. Rate limited; wait for the next sync cycle.");
        }
        if self
            .cancel
            .as_ref()
            .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        {
            bail!("Sync stopped; pending local changes will be retried next time");
        }
        let mut request = self.client.request(method, format!("{}{path}", self.base));
        if self.cancel.is_some() {
            request = request.timeout(Duration::from_secs(8));
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .context("ClickUp request failed; check connectivity (writes may have succeeded)")?;
        let status = response.status();
        if !status.is_success() {
            if status.as_u16() == 429 {
                self.rate_limited.set(true);
            }
            bail!(
                "ClickUp returned HTTP {}. {}",
                status.as_u16(),
                match status.as_u16() {
                    401 | 403 => "Check your token and List permissions.",
                    404 => "The task/List is missing or inaccessible; nothing will be deleted.",
                    429 => "Rate limited. Wait before syncing again.",
                    _ => "Inspect ClickUp before retrying a write.",
                }
            );
        }
        response
            .json()
            .context("ClickUp returned an invalid response")
    }

    fn get(&self, path: &str) -> Result<Value> {
        self.request(Method::GET, path, None)
    }

    fn remote(&self, id: &str) -> Result<Remote> {
        identifier(id)?;
        serde_json::from_value(self.get(&format!("/task/{id}?include_markdown_description=true"))?)
            .context("Invalid ClickUp task")
    }

    fn tasks(&self, list: &str) -> Result<Vec<Remote>> {
        let mut tasks = Vec::new();
        let mut seen = HashSet::new();
        for page in 0..10_000 {
            let value = self.get(&format!("/list/{list}/task?include_closed=true&subtasks=true&include_markdown_description=true&page={page}"))?;
            let batch: Vec<Remote> = serde_json::from_value(
                value
                    .get("tasks")
                    .context("ClickUp response is missing tasks")?
                    .clone(),
            )?;
            let count = batch.len();
            for task in batch {
                if !seen.insert(task.id.clone()) {
                    bail!("ClickUp pagination repeated a task; retry after remote edits settle");
                }
                tasks.push(task);
            }
            if value.get("last_page").and_then(Value::as_bool) == Some(true) || count == 0 {
                return Ok(tasks);
            }
        }
        bail!("ClickUp pagination limit reached; no sync changes were applied")
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct RemoteStatus {
    status: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Remote {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    markdown_description: Option<String>,
    status: RemoteStatus,
    priority: Option<Value>,
    due_date: Option<Value>,
    #[serde(default)]
    due_date_time: Option<bool>,
    list: Value,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    date_closed: Option<Value>,
    #[serde(default)]
    start_date: Option<Value>,
    #[serde(default)]
    start_date_time: Option<bool>,
    #[serde(default)]
    assignees: Vec<Assignee>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
struct State {
    title: String,
    notes: String,
    priority: Priority,
    deadline: Option<String>,
    done: bool,
    in_progress: bool,
    #[serde(default)]
    start_date: Option<String>,
    #[serde(default)]
    assignees: Vec<i64>,
}

impl From<&Task> for State {
    fn from(task: &Task) -> Self {
        Self {
            title: task.title.clone(),
            notes: task.notes.clone(),
            priority: task.priority,
            deadline: task.deadline.clone(),
            done: task.done,
            in_progress: task.in_progress,
            start_date: task.start_date.clone(),
            assignees: {
                let mut ids: Vec<_> = task.assignees.iter().map(|a| a.id).collect();
                ids.sort_unstable();
                ids.dedup();
                ids
            },
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct Link {
    project_id: i64,
    list_id: String,
    open_status: String,
    working_status: String,
    done_status: String,
    timezone: Tz,
}

// Preserve remote time precision and custom statuses on unrelated edits.
#[derive(Clone, PartialEq, Deserialize, Serialize)]
struct RemoteBase {
    state: State,
    status: String,
    priority: Option<Value>,
    due_date: Option<Value>,
    due_date_time: Option<bool>,
    completed_at: Option<String>,
    #[serde(default)]
    start_date: Option<Value>,
    #[serde(default)]
    start_date_time: Option<bool>,
    #[serde(default)]
    assignees: Vec<Assignee>,
}

impl Remote {
    fn snapshot(&self, link: &Link) -> Result<RemoteBase> {
        identifier(&self.id)?;
        if self.list.get("id").and_then(Value::as_str) != Some(link.list_id.as_str())
            || self.archived
        {
            bail!(
                "Task {} moved Lists or is archived; no changes applied",
                self.id
            );
        }
        let priority_id = self
            .priority
            .as_ref()
            .and_then(|v| v.get("id"))
            .and_then(Value::as_str);
        let deadline = self
            .due_date
            .as_ref()
            .filter(|v| !v.is_null())
            .map(|value| {
                let millis = value
                    .as_i64()
                    .or_else(|| value.as_str().and_then(|v| v.parse().ok()))
                    .context("Invalid ClickUp due date")?;
                let date = Utc
                    .timestamp_millis_opt(millis)
                    .single()
                    .context("ClickUp due date is out of range")?
                    .with_timezone(&link.timezone)
                    .date_naive();
                if !(1..=9999).contains(&chrono::Datelike::year(&date)) {
                    bail!("ClickUp due date is out of range");
                }
                Ok(date.format("%Y-%m-%d").to_string())
            })
            .transpose()?;
        let done = matches!(self.status.kind.as_str(), "closed" | "done")
            || self.status.status == link.done_status;
        Ok(RemoteBase {
            state: State {
                title: self.name.clone(),
                notes: self
                    .markdown_description
                    .as_ref()
                    .or(self.description.as_ref())
                    .cloned()
                    .unwrap_or_default(),
                priority: match priority_id {
                    Some("1") => Priority::Urgent,
                    Some("2") => Priority::High,
                    Some("4") => Priority::Low,
                    Some("3") => Priority::Normal,
                    _ => Priority::None,
                },
                deadline,
                done,
                in_progress: !done && self.status.status == link.working_status,
                start_date: remote_date(self.start_date.as_ref(), link.timezone)?,
                assignees: {
                    let mut ids: Vec<_> = self.assignees.iter().map(|a| a.id).collect();
                    ids.sort_unstable();
                    ids.dedup();
                    ids
                },
            },
            status: self.status.status.clone(),
            priority: self.priority.clone(),
            due_date: self.due_date.clone(),
            due_date_time: self.due_date_time,
            start_date: self.start_date.clone(),
            start_date_time: self.start_date_time,
            assignees: self.assignees.clone(),
            completed_at: if done {
                self.date_closed
                    .as_ref()
                    .and_then(|v| {
                        v.as_i64()
                            .or_else(|| v.as_str().and_then(|v| v.parse().ok()))
                    })
                    .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
                    .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            } else {
                None
            },
        })
    }
}

fn remote_date(value: Option<&Value>, timezone: Tz) -> Result<Option<String>> {
    value
        .filter(|v| !v.is_null())
        .map(|value| {
            let millis = value
                .as_i64()
                .or_else(|| value.as_str().and_then(|v| v.parse().ok()))
                .context("Invalid ClickUp date")?;
            let date = Utc
                .timestamp_millis_opt(millis)
                .single()
                .context("ClickUp date is out of range")?
                .with_timezone(&timezone)
                .date_naive();
            if !(1..=9999).contains(&chrono::Datelike::year(&date)) {
                bail!("ClickUp date is out of range");
            }
            Ok(date.format("%Y-%m-%d").to_string())
        })
        .transpose()
}

fn date_millis(date: &Option<String>, timezone: Tz) -> Result<Option<i64>> {
    date.as_ref()
        .map(|d| {
            let date = NaiveDate::parse_from_str(d, "%Y-%m-%d")?;
            Ok(timezone
                .from_local_datetime(&date.and_hms_opt(4, 0, 0).unwrap())
                .single()
                .context("Date is ambiguous in your ClickUp timezone")?
                .timestamp_millis())
        })
        .transpose()
}

fn payload(state: &State, old: Option<&State>, link: &Link) -> Result<Value> {
    let mut body = Map::new();
    if old.is_none_or(|o| o.title != state.title) {
        body.insert("name".into(), json!(state.title));
    }
    if old.is_none_or(|o| o.notes != state.notes) {
        body.insert("markdown_content".into(), json!(state.notes));
    }
    if old.is_none_or(|o| o.priority != state.priority) {
        body.insert(
            "priority".into(),
            json!(match state.priority {
                Priority::Urgent => Some(1),
                Priority::High => Some(2),
                Priority::Normal => Some(3),
                Priority::Low => Some(4),
                Priority::None => None,
            }),
        );
    }
    if old.is_none_or(|o| o.start_date != state.start_date) {
        let millis = date_millis(&state.start_date, link.timezone)?;
        if old.is_some() || millis.is_some() {
            body.insert("start_date".into(), json!(millis));
            body.insert("start_date_time".into(), json!(false));
        }
    }
    if old.is_none_or(|o| o.assignees != state.assignees) {
        if let Some(old) = old {
            body.insert("assignees".into(), json!({
                "add": state.assignees.iter().filter(|id| !old.assignees.contains(id)).collect::<Vec<_>>(),
                "rem": old.assignees.iter().filter(|id| !state.assignees.contains(id)).collect::<Vec<_>>(),
            }));
        } else {
            body.insert("assignees".into(), json!(state.assignees));
        }
    }
    if old.is_none_or(|o| o.done != state.done || o.in_progress != state.in_progress) {
        body.insert(
            "status".into(),
            json!(if state.done {
                &link.done_status
            } else if state.in_progress {
                &link.working_status
            } else {
                &link.open_status
            }),
        );
    }
    if old.is_none_or(|o| o.deadline != state.deadline) {
        let millis = state
            .deadline
            .as_ref()
            .map(|d| {
                let date = NaiveDate::parse_from_str(d, "%Y-%m-%d")?;
                let dt = link
                    .timezone
                    .from_local_datetime(&date.and_hms_opt(4, 0, 0).unwrap())
                    .single()
                    .context("Deadline is ambiguous in your ClickUp timezone")?;
                Ok::<_, anyhow::Error>(dt.timestamp_millis())
            })
            .transpose()?;
        if old.is_some() || millis.is_some() {
            body.insert("due_date".into(), json!(millis));
            body.insert("due_date_time".into(), json!(false));
        }
    }
    if old.is_none() {
        body.insert("check_required_custom_fields".into(), json!(true));
    }
    Ok(Value::Object(body))
}

struct Mapping {
    id: i64,
    local_id: Option<i64>,
    remote_id: Option<String>,
    local_base: Option<State>,
    remote_base: Option<RemoteBase>,
    pending: bool,
}

fn mappings(store: &Store, project: i64) -> Result<Vec<Mapping>> {
    let mut stmt = store.conn.prepare("SELECT id, local_id, remote_id, local_base, remote_base, pending FROM clickup_tasks WHERE project_id = ?1")?;
    let rows = stmt.query_map([project], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, bool>(5)?,
        ))
    })?;
    rows.map(|row| {
        let (id, local_id, remote_id, local, remote, pending) = row?;
        Ok(Mapping {
            id,
            local_id,
            remote_id,
            local_base: local.map(|s| serde_json::from_str(&s)).transpose()?,
            remote_base: remote.map(|s| serde_json::from_str(&s)).transpose()?,
            pending,
        })
    })
    .collect()
}

fn links(store: &Store) -> Result<Vec<Link>> {
    let mut stmt = store.conn.prepare("SELECT project_id, list_id, open_status, working_status, done_status, timezone FROM clickup_projects ORDER BY project_id")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    rows.map(|row| {
        let (project_id, list_id, open_status, working_status, done_status, timezone) = row?;
        Ok(Link {
            project_id,
            list_id,
            open_status,
            working_status,
            done_status,
            timezone: timezone.parse()?,
        })
    })
    .collect()
}

fn atomic<T>(store: &Store, f: impl FnOnce() -> Result<T>) -> Result<T> {
    store.conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = f();
    match result {
        Ok(value) => {
            if let Err(error) = store.conn.execute_batch("COMMIT") {
                let _ = store.conn.execute_batch("ROLLBACK");
                return Err(error.into());
            }
            Ok(value)
        }
        Err(error) => {
            let _ = store.conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn apply(store: &Store, id: i64, state: &State) -> Result<Task> {
    store.edit_task(
        id,
        TaskEdit {
            title: Some(state.title.clone()),
            notes: Some(state.notes.clone()),
            priority: Some(state.priority),
            deadline: Some(state.deadline.clone()),
            project: None,
            start_date: Some(state.start_date.clone()),
            ..TaskEdit::default()
        },
    )?;
    store.set_done(id, state.done)?;
    if state.in_progress {
        store.set_working(id, true)?;
    }
    store.task(id)
}

fn apply_remote(store: &Store, id: i64, snapshot: &RemoteBase) -> Result<Task> {
    apply(store, id, &snapshot.state)?;
    store.conn.execute(
        "UPDATE tasks SET assignees = ?1 WHERE id = ?2",
        params![serde_json::to_string(&snapshot.assignees)?, id],
    )?;
    if snapshot.state.done {
        store.conn.execute(
            "UPDATE tasks SET completed_at = ?1 WHERE id = ?2",
            params![snapshot.completed_at, id],
        )?;
    }
    store.task(id)
}

fn baseline(
    store: &Store,
    mapping: i64,
    local: &Task,
    remote: &RemoteBase,
    remote_id: &str,
) -> Result<()> {
    store.conn.execute(
        "UPDATE clickup_tasks SET remote_id = ?1, local_base = ?2, remote_base = ?3, pending = 0 WHERE id = ?4",
        params![remote_id, serde_json::to_string(&State::from(local))?, serde_json::to_string(remote)?, mapping],
    )?;
    Ok(())
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Choice {
    Local,
    Remote,
}

#[derive(Default, Serialize)]
struct Report {
    imported: usize,
    created: usize,
    pulled: usize,
    pushed: usize,
    unchanged: usize,
    issues: Vec<Issue>,
}

#[derive(Serialize)]
struct Issue {
    task_id: Option<i64>,
    remote_id: Option<String>,
    message: String,
}

fn sync(store: &Store, api: &Api, link: &Link, resolve: Option<(i64, Choice)>) -> Result<Report> {
    let metadata = api.get(&format!("/list/{}", link.list_id))?;
    validate_statuses(&metadata, link)?;
    refresh_members(store, api, link)?;
    let remote_tasks = api.tasks(&link.list_id)?;
    let mut report = Report::default();
    let known = mappings(store, link.project_id)?;
    let has_pending = known.iter().any(|m| m.pending);
    if let Some((id, _)) = resolve {
        if !known
            .iter()
            .any(|m| m.local_id == Some(id) && m.remote_id.is_some())
        {
            bail!(
                "Task {id} is not a synced task in project {}",
                link.project_id
            );
        }
    }
    // Tombstones remain mapped: deleting locally must not resurrect tasks on pull.
    let seen_remote: HashSet<String> = known.iter().filter_map(|m| m.remote_id.clone()).collect();
    for mapping in known {
        let result = sync_one(store, api, link, &mapping, resolve, &mut report);
        if let Err(error) = result {
            report.issues.push(Issue {
                task_id: mapping.local_id,
                remote_id: mapping.remote_id,
                message: format!("{error:#}"),
            });
        }
    }
    for remote in remote_tasks {
        if seen_remote.contains(&remote.id) {
            continue;
        }
        if has_pending {
            report.issues.push(Issue { task_id: None, remote_id: Some(remote.id),
                message: "Import deferred until uncertain creations are recovered, to prevent duplicate tasks".into() });
            continue;
        }
        let result = atomic(store, || {
            let snapshot = remote.snapshot(link)?;
            let task = store.add_task(
                &snapshot.state.title,
                &snapshot.state.notes,
                Some(link.project_id),
                snapshot.state.priority,
                snapshot.state.deadline.as_deref(),
            )?;
            let task = apply_remote(store, task.id, &snapshot)?;
            store.conn.execute(
                "INSERT INTO clickup_tasks(local_id, project_id, remote_id) VALUES (?1, ?2, ?3)",
                params![task.id, link.project_id, remote.id],
            )?;
            baseline(
                store,
                store.conn.last_insert_rowid(),
                &task,
                &snapshot,
                &remote.id,
            )?;
            Ok(())
        });
        match result {
            Ok(()) => report.imported += 1,
            Err(error) => report.issues.push(Issue {
                task_id: None,
                remote_id: Some(remote.id),
                message: format!("{error:#}"),
            }),
        }
    }
    let mapped: HashSet<i64> = mappings(store, link.project_id)?
        .iter()
        .filter_map(|m| m.local_id)
        .collect();
    for task in store.tasks(Scope::Project(link.project_id), Status::All, "")? {
        if mapped.contains(&task.id) {
            continue;
        }
        // A task mapped to a different project is a move, not a new remote task.
        if store.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM clickup_tasks WHERE local_id = ?1)",
            [task.id],
            |r| r.get::<_, bool>(0),
        )? {
            continue;
        }
        let result = create_remote(store, api, link, task.id);
        match result {
            Ok(()) => report.created += 1,
            Err(error) => report.issues.push(Issue {
                task_id: Some(task.id),
                remote_id: None,
                message: format!("{error:#}"),
            }),
        }
    }
    Ok(report)
}

fn refresh_members(store: &Store, api: &Api, link: &Link) -> Result<Vec<Assignee>> {
    let response = api.get(&format!("/list/{}/member", link.list_id))?;
    let members: Vec<Assignee> = serde_json::from_value(
        response
            .get("members")
            .context("Missing List members")?
            .clone(),
    )?;
    store.conn.execute(
        "UPDATE clickup_projects SET members=?1 WHERE project_id=?2",
        params![serde_json::to_string(&members)?, link.project_id],
    )?;
    Ok(members)
}

fn store_location(store: &Store, list: &DiscoveredList) -> Result<()> {
    store.conn.execute(
        "UPDATE clickup_projects SET workspace=?1,space=?2,folder=?3,list_name=?4 WHERE list_id=?5",
        params![list.workspace, list.space, list.folder, list.name, list.id],
    )?;
    Ok(())
}

fn automatic_link(store: &Store, api: &Api, list: &DiscoveredList, timezone: Tz) -> Result<Link> {
    if let Some(link) = links(store)?.into_iter().find(|l| l.list_id == list.id) {
        store_location(store, list)?;
        return Ok(link);
    }
    let metadata = api.get(&format!("/list/{}", list.id))?;
    let statuses = metadata["statuses"]
        .as_array()
        .context("List has no statuses")?;
    let pick = |names: &[&str], completed: bool| -> Result<String> {
        let candidates: Vec<_> = statuses
            .iter()
            .filter(|s| matches!(s["type"].as_str(), Some("done" | "closed")) == completed)
            .filter_map(|s| s["status"].as_str())
            .collect();
        let matches: Vec<_> = candidates
            .iter()
            .filter(|c| names.iter().any(|n| c.eq_ignore_ascii_case(n)))
            .collect();
        if matches.len() == 1 {
            return Ok((**matches[0]).to_owned());
        }
        if completed && candidates.len() == 1 {
            return Ok(candidates[0].to_owned());
        }
        bail!(
            "Ambiguous status mapping for '{}'; press C to choose statuses once",
            crate::safe(&list.name)
        )
    };
    let mut link = Link {
        project_id: 0,
        list_id: list.id.clone(),
        timezone,
        open_status: pick(&["to do", "open", "backlog", "not started"], false)?,
        working_status: pick(&["in progress", "doing", "working"], false)?,
        done_status: pick(&["complete", "completed", "closed", "done"], true)?,
    };
    validate_statuses(&metadata, &link)?;
    atomic(store, || {
        let path = format!(
            "ClickUp: {} / {} / {}",
            list.workspace, list.space, list.name
        );
        let name = if store
            .projects()?
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(&path))
        {
            format!("{path} [{}]", list.id)
        } else {
            path
        };
        link.project_id = store.add_project(&name)?.id;
        store.conn.execute("INSERT INTO clickup_projects(project_id,list_id,open_status,working_status,done_status,timezone) VALUES (?1,?2,?3,?4,?5,?6)",
            params![link.project_id,link.list_id,link.open_status,link.working_status,link.done_status,timezone.to_string()])?;
        store_location(store, list)?;
        Ok(())
    })?;
    Ok(link)
}

#[derive(Default, Serialize)]
pub struct AutoReport {
    pub projects: usize,
    pub changed: usize,
    pub issues: Vec<String>,
}

pub fn auto_enabled(store: &Store) -> Result<bool> {
    Ok(store.conn.query_row(
        "SELECT enabled FROM clickup_preferences WHERE id=1",
        [],
        |r| r.get(0),
    )?)
}

fn auto_cycle_with_api(store: &Store, api: &Api, discover_new: bool) -> Result<AutoReport> {
    let mut result = AutoReport::default();
    if !auto_enabled(store)? {
        return Ok(result);
    }
    let all_lists: bool = store.conn.query_row(
        "SELECT all_lists FROM clickup_preferences WHERE id=1",
        [],
        |r| r.get(0),
    )?;
    if discover_new && all_lists {
        let profile = api.get("/user")?;
        let configured: Option<String> = store.conn.query_row(
            "SELECT timezone FROM clickup_preferences WHERE id=1",
            [],
            |r| r.get(0),
        )?;
        let timezone = configured
            .as_deref()
            .and_then(|v| v.parse::<Tz>().ok())
            .or_else(|| {
                profile["user"]["timezone"]
                    .as_str()
                    .and_then(|v| v.parse().ok())
            })
            .or(links(store)?.first().map(|l| l.timezone))
            .context(
                "Set your ClickUp timezone once: todo clickup auto --timezone Europe/Stockholm",
            )?;
        let discovery = discover(api)?;
        result.issues.extend(discovery.warnings);
        for list in discovery.lists {
            if let Err(error) = automatic_link(store, api, &list, timezone) {
                result
                    .issues
                    .push(format!("{}: {error:#}", crate::safe(&list.name)));
            }
        }
    }
    for link in links(store)? {
        result.projects += 1;
        match sync(store, api, &link, None) {
            Ok(report) => {
                result.changed += report.created + report.imported + report.pushed + report.pulled;
                result.issues.extend(report.issues.into_iter().map(|i| {
                    format!(
                        "Project {} task {:?}: {}",
                        link.project_id, i.task_id, i.message
                    )
                }));
            }
            Err(error) => result
                .issues
                .push(format!("Project {}: {error:#}", link.project_id)),
        }
    }
    Ok(result)
}

fn auto_cycle(
    database: &Path,
    discover_new: bool,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<AutoReport> {
    let store = Store::open(database)?;
    if !auto_enabled(&store)? {
        return Ok(AutoReport::default());
    }
    let secret = match token() {
        Ok(token) => token,
        Err(_) if links(&store)?.is_empty() => return Ok(AutoReport::default()),
        Err(error) => return Err(error),
    };
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(database.canonicalize()?.with_extension("clickup.lock"))?;
    lock.try_lock_exclusive()
        .context("Another ClickUp operation is running; will retry")?;
    let mut api = Api::new(&secret)?;
    api.cancel = cancel;
    auto_cycle_with_api(&store, &api, discover_new)
}

pub fn push_after_edit(database: &Path) -> Result<AutoReport> {
    auto_cycle(database, false, None)
}

pub struct AutoSync {
    requests: Option<std::sync::mpsc::Sender<bool>>,
    pub results: std::sync::mpsc::Receiver<Result<AutoReport>>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AutoSync {
    pub fn start(database: &Path) -> Self {
        let (requests, receive) = std::sync::mpsc::channel::<bool>();
        let (send, results) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = cancel.clone();
        let path = database.to_path_buf();
        let thread = std::thread::spawn(move || {
            while let Ok(mut discover) = receive.recv() {
                // Coalesce rapid edits, but never lose a requested discovery pass.
                std::thread::sleep(Duration::from_millis(200));
                while let Ok(request) = receive.try_recv() {
                    discover |= request;
                }
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let result = auto_cycle(&path, discover, Some(stop.clone()));
                if send.send(result).is_err() {
                    break;
                }
            }
        });
        let worker = Self {
            requests: Some(requests),
            results,
            cancel,
            thread: Some(thread),
        };
        worker.request(true);
        worker
    }

    pub fn request(&self, discovery: bool) {
        if let Some(sender) = &self.requests {
            let _ = sender.send(discovery);
        }
    }
}

impl Drop for AutoSync {
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.requests.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn sync_one(
    store: &Store,
    api: &Api,
    link: &Link,
    mapping: &Mapping,
    resolve: Option<(i64, Choice)>,
    report: &mut Report,
) -> Result<()> {
    let Some(id) = mapping.local_id else {
        return Ok(());
    };
    if mapping.pending {
        bail!("Creation outcome uncertain. Inspect ClickUp, then run `todo clickup recover {id} REMOTE_ID`. Do not retry creation blindly");
    }
    let remote_id = mapping
        .remote_id
        .as_deref()
        .context("Missing remote task ID")?;
    // Fetch freshly rather than use a possibly stale paginated List snapshot.
    let remote = api.remote(remote_id)?.snapshot(link)?;
    {
        let local = store.task(id)?;
        if local.project_id != Some(link.project_id) {
            bail!("Task moved out of the linked project; move it back to resume sync (remote List moves are not automatic)");
        }
        let state = State::from(&local);
        let old_local = mapping
            .local_base
            .as_ref()
            .context("Missing local sync baseline")?;
        let old_remote = mapping
            .remote_base
            .as_ref()
            .context("Missing remote sync baseline")?;
        let local_changed = &state != old_local;
        let remote_changed = &remote != old_remote;
        let choice = resolve
            .filter(|(task, _)| *task == id)
            .map(|(_, choice)| choice);
        if local_changed && remote_changed && state != remote.state && choice.is_none() {
            bail!("Conflict: both copies changed. Inspect both, then sync with --prefer-local {id} or --prefer-remote {id}");
        }
        let push = matches!(choice, Some(Choice::Local))
            || (choice.is_none() && local_changed && !remote_changed);
        let pull = matches!(choice, Some(Choice::Remote))
            || (choice.is_none() && remote_changed && !local_changed);
        if push {
            // ClickUp has no conditional update/ETag API. Check again immediately
            // before writing; a remote edit during the request is still possible.
            if api.remote(remote_id)?.snapshot(link)? != remote {
                bail!("ClickUp changed during sync; retry before resolving");
            }
            let body = payload(&state, Some(&remote.state), link)?;
            if !body.as_object().unwrap().is_empty() {
                api.request(Method::PUT, &format!("/task/{remote_id}"), Some(&body))?;
            }
            let saved = api.remote(remote_id)?.snapshot(link)?;
            if saved.state != state {
                bail!("ClickUp did not retain the requested values; baseline left unchanged");
            }
            atomic(store, || {
                baseline(store, mapping.id, &local, &saved, remote_id)
            })?;
            report.pushed += 1;
        } else if pull {
            atomic(store, || {
                let current = store.task(id)?;
                if current.project_id != Some(link.project_id) || State::from(&current) != state {
                    bail!("Task edited locally during sync; pull deferred");
                }
                let task = apply_remote(store, id, &remote)?;
                baseline(store, mapping.id, &task, &remote, remote_id)
            })?;
            report.pulled += 1;
        } else {
            atomic(store, || {
                baseline(store, mapping.id, &local, &remote, remote_id)
            })?;
            report.unchanged += 1;
        }
        Ok(())
    }
}

fn create_remote(store: &Store, api: &Api, link: &Link, id: i64) -> Result<()> {
    let (task, mapping) = atomic(store, || {
        let task = store.task(id)?;
        if task.project_id != Some(link.project_id) {
            bail!("Task moved during sync; retry");
        }
        payload(&State::from(&task), None, link)?;
        store.conn.execute(
            "INSERT INTO clickup_tasks(local_id, project_id, pending) VALUES (?1, ?2, 1)",
            params![id, link.project_id],
        )?;
        Ok((task, store.conn.last_insert_rowid()))
    })?;
    // The durable pending record intentionally survives errors or process death,
    // since ClickUp creation has no idempotency key.
    let body = payload(&State::from(&task), None, link)?;
    let value = api
        .request(
            Method::POST,
            &format!("/list/{}/task", link.list_id),
            Some(&body),
        )
        .with_context(|| {
            format!(
                "Task {id} creation may have succeeded; inspect ClickUp and use `clickup recover`"
            )
        })?;
    let remote_id = value
        .get("id")
        .and_then(Value::as_str)
        .context("Create response missing its task ID; use `clickup recover`")?;
    identifier(remote_id)?;
    store.conn.execute(
        "UPDATE clickup_tasks SET remote_id = ?1 WHERE id = ?2",
        params![remote_id, mapping],
    )?;
    // Create responses may omit Markdown. Read the canonical task before
    // recording a baseline so formatting does not become a false conflict.
    let remote = api.remote(remote_id)?;
    let snapshot = remote.snapshot(link)?;
    atomic(store, || {
        baseline(store, mapping, &task, &snapshot, &remote.id)?;
        if snapshot.state != State::from(&task) {
            store.conn.execute(
                "UPDATE clickup_tasks SET local_base = ?1, remote_base = ?2 WHERE id = ?3",
                params![
                    serde_json::to_string(&snapshot.state)?,
                    serde_json::to_string(&RemoteBase {
                        state: State::from(&task),
                        ..snapshot
                    })?,
                    mapping
                ],
            )?;
        }
        Ok(())
    })?;
    Ok(())
}

fn recover(store: &Store, api: &Api, task: i64, remote_id: &str) -> Result<()> {
    identifier(remote_id)?;
    let project: i64 = store
        .conn
        .query_row(
            "SELECT project_id FROM clickup_tasks WHERE local_id = ?1 AND pending = 1",
            [task],
            |r| r.get(0),
        )
        .context("Task has no pending ClickUp creation")?;
    let link = links(store)?
        .into_iter()
        .find(|l| l.project_id == project)
        .context("Missing project link")?;
    let remote = api.remote(remote_id)?.snapshot(&link)?;
    atomic(store, || {
        let local = store.task(task)?;
        let mapping: i64 = store.conn.query_row(
            "SELECT id FROM clickup_tasks WHERE local_id = ?1",
            [task],
            |r| r.get(0),
        )?;
        // Recovery attaches identity only; divergent baselines force an explicit
        // conflict if either copy differs, rather than silently choosing a winner.
        store.conn.execute(
            "UPDATE clickup_tasks SET remote_id = ?1, pending = 0, local_base = ?2, remote_base = ?3 WHERE id = ?4",
            params![remote_id, serde_json::to_string(&remote.state)?,
                serde_json::to_string(&RemoteBase { state: State::from(&local), ..remote })?, mapping])?;
        Ok(())
    })
}

fn validate_statuses(metadata: &Value, link: &Link) -> Result<()> {
    let statuses = metadata
        .get("statuses")
        .and_then(Value::as_array)
        .context("ClickUp List has no statuses")?;
    if link.open_status == link.working_status
        || link.open_status == link.done_status
        || link.working_status == link.done_status
    {
        bail!("Open, working and done statuses must be different");
    }
    for (name, completed) in [
        (&link.open_status, false),
        (&link.working_status, false),
        (&link.done_status, true),
    ] {
        let status = statuses
            .iter()
            .find(|s| s.get("status").and_then(Value::as_str) == Some(name.as_str()))
            .with_context(|| {
                format!(
                    "Status '{}' not found in ClickUp List; inspect `todo clickup list {}`",
                    crate::safe(name),
                    link.list_id
                )
            })?;
        let kind = status
            .get("type")
            .and_then(Value::as_str)
            .context("Status has no type")?;
        if matches!(kind, "closed" | "done") != completed {
            bail!(
                "Status '{}' has an incompatible completion type",
                crate::safe(name)
            );
        }
    }
    Ok(())
}

pub fn run(command: Command, database: &Path, json_output: bool) -> Result<()> {
    if matches!(command, Command::Import)
        && (json_output || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal())
    {
        bail!("import requires an interactive terminal; use --json clickup discover and clickup link/sync for automation");
    }
    match command {
        Command::Connect { no_browser } => {
            if json_output || !std::io::stdin().is_terminal() {
                bail!("connect requires an interactive terminal; scripts should set CLICKUP_API_TOKEN");
            }
            eprintln!("Generate/copy your personal token at {TOKEN_URL}");
            if !no_browser {
                let program = if cfg!(target_os = "macos") {
                    "open"
                } else {
                    "xdg-open"
                };
                match std::process::Command::new(program).arg(TOKEN_URL).status() {
                    Ok(status) if status.success() => {}
                    _ => eprintln!("Could not open browser; open the URL above manually."),
                }
            }
            let secret = rpassword::prompt_password("Paste ClickUp API token (hidden): ")?;
            Api::new(secret.trim())?
                .get("/user")
                .context("Token validation failed; credential was not saved")?;
            credential()?.set_password(secret.trim()).context("Cannot save token to OS credential store; set CLICKUP_API_TOKEN instead (no plaintext fallback)")?;
            println!("ClickUp token saved to your OS credential store.");
            println!("Start todo to discover/import accessible Lists automatically. Press C for manual List/status setup.");
            return Ok(());
        }
        Command::Disconnect => {
            match credential()?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(error) => return Err(error).context("Cannot remove saved ClickUp credential"),
            }
            return crate::output(json_output, &json!({"disconnected":true}), || {
                println!("Saved token removed. Revoke it in ClickUp settings if needed; CLICKUP_API_TOKEN still takes precedence.")
            });
        }
        _ => {}
    }
    let store = Store::open(database)?;
    if let Command::Auto {
        disable,
        enable,
        timezone,
    } = command
    {
        if disable || enable {
            store.conn.execute(
                "UPDATE clickup_preferences SET enabled=?1 WHERE id=1",
                [!disable],
            )?;
        }
        if let Some(timezone) = timezone {
            store.conn.execute(
                "UPDATE clickup_preferences SET timezone=?1 WHERE id=1",
                [timezone.to_string()],
            )?;
        }
        let enabled = auto_enabled(&store)?;
        return crate::output(
            json_output,
            &json!({"automatic_sync":enabled,"all_accessible_lists":true}),
            || {
                println!("Automatic ClickUp sync: {}. Discovers all accessible Lists while todo is open.", if enabled { "on" } else { "off" })
            },
        );
    }
    if matches!(command, Command::Links) {
        let links = links(&store)?;
        return crate::output(json_output, &links, || {
            for link in &links {
                println!(
                    "Project {} -> ClickUp List {} ({})",
                    link.project_id, link.list_id, link.timezone
                );
            }
        });
    }
    let lock_path = database.canonicalize()?.with_extension("clickup.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock.try_lock_exclusive()
        .context("Another ClickUp operation is running for this database")?;
    if let Command::RetryCreate {
        task,
        confirm_missing: true,
    } = command
    {
        if store.conn.execute(
            "DELETE FROM clickup_tasks WHERE local_id = ?1 AND pending = 1",
            [task],
        )? != 1
        {
            bail!("Task has no pending ClickUp creation");
        }
        return crate::output(
            json_output,
            &json!({"task_id":task,"retry_allowed":true}),
            || println!("Cleared pending creation for {task}. Next sync may create it again."),
        );
    }
    let api = Api::new(&token()?)?;
    match command {
        Command::Members { project } => {
            let link = links(&store)?
                .into_iter()
                .find(|l| l.project_id == project)
                .context("Not a ClickUp project")?;
            let members = refresh_members(&store, &api, &link)?;
            crate::output(json_output, &members, || {
                for member in &members {
                    println!("{}  {}", member.id, crate::safe(&member.label()));
                }
            })
        }
        Command::Discover => {
            let discovery = discover(&api)?;
            crate::output(json_output, &discovery, || {
                for list in &discovery.lists {
                    println!(
                        "{}  {} / {} / {}{}",
                        list.id,
                        crate::safe(&list.workspace),
                        crate::safe(&list.space),
                        list.folder
                            .as_ref()
                            .map(|f| format!("{} / ", crate::safe(f)))
                            .unwrap_or_default(),
                        crate::safe(&list.name)
                    );
                }
                for warning in &discovery.warnings {
                    eprintln!("Warning: {}", crate::safe(warning));
                }
            })
        }
        Command::Import => interactive_import(&store, &api),
        Command::List { list_id } => {
            identifier(&list_id)?;
            let value = api.get(&format!("/list/{list_id}"))?;
            crate::output(json_output, &value, || {
                println!(
                    "{}",
                    crate::safe(
                        value
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("ClickUp List")
                    )
                );
                if let Some(statuses) = value.get("statuses").and_then(Value::as_array) {
                    for status in statuses {
                        println!(
                            "{} ({})",
                            crate::safe(status["status"].as_str().unwrap_or("?")),
                            crate::safe(status["type"].as_str().unwrap_or("?"))
                        );
                    }
                }
            })
        }
        Command::Link {
            project,
            list_id,
            timezone,
            open_status,
            working_status,
            done_status,
        } => {
            identifier(&list_id)?;
            let link = Link {
                project_id: project,
                list_id,
                open_status,
                working_status,
                done_status,
                timezone,
            };
            let metadata = api.get(&format!("/list/{}", link.list_id))?;
            validate_statuses(&metadata, &link)?;
            store.conn.execute("INSERT INTO clickup_projects(project_id,list_id,open_status,working_status,done_status,timezone) VALUES (?1,?2,?3,?4,?5,?6)",
                params![project, link.list_id, link.open_status, link.working_status, link.done_status, timezone.to_string()])
                .context("Cannot link: project must exist, and each project/List can be linked only once")?;
            crate::output(json_output, &link, || {
                println!("Linked project {project}. `todo clickup sync {project}` will exchange all tasks in this project and List.")
            })
        }
        Command::Sync {
            project,
            prefer_local,
            prefer_remote,
        } => {
            let link = links(&store)?
                .into_iter()
                .find(|l| l.project_id == project)
                .context("Project is not linked to ClickUp")?;
            let resolve = prefer_local
                .map(|id| (id, Choice::Local))
                .or(prefer_remote.map(|id| (id, Choice::Remote)));
            let report = sync(&store, &api, &link, resolve)?;
            crate::output(json_output, &report, || {
                println!(
                    "Imported {} | Created {} | Pulled {} | Pushed {} | Unchanged {}",
                    report.imported, report.created, report.pulled, report.pushed, report.unchanged
                );
                for issue in &report.issues {
                    println!(
                        "Task {:?} / ClickUp {}: {}",
                        issue.task_id,
                        issue.remote_id.as_deref().unwrap_or("-"),
                        crate::safe(&issue.message)
                    );
                }
            })?;
            if !report.issues.is_empty() {
                bail!(
                    "Sync completed with {} unresolved issue(s); see report",
                    report.issues.len()
                );
            }
            Ok(())
        }
        Command::Recover { task, remote_id } => {
            recover(&store, &api, task, &remote_id)?;
            crate::output(
                json_output,
                &json!({"task_id":task,"remote_id":remote_id,"recovered":true}),
                || {
                    println!("Linked recovery task {task}. Sync again; differing copies will be reported as a conflict.")
                },
            )
        }
        Command::Connect { .. }
        | Command::Disconnect
        | Command::Links
        | Command::Auto { .. }
        | Command::RetryCreate { .. } => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::TcpListener,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        thread::{self, JoinHandle},
    };

    fn remote(id: &str, name: &str) -> Value {
        json!({
            "id":id, "name":name, "markdown_description":"**notes**",
            "description":"notes", "status":{"status":"to do","type":"open"},
            "priority":{"id":"1","priority":"urgent"}, "due_date":null,
            "due_date_time":false, "list":{"id":"123"}, "date_closed":null
        })
    }

    fn metadata() -> Value {
        json!({"id":"123","name":"Work","statuses":[
            {"status":"to do","type":"open"},
            {"status":"in progress","type":"custom"},
            {"status":"closed","type":"closed"}
        ]})
    }

    #[derive(Default)]
    struct Backend {
        responses: BTreeMap<String, (u16, Value)>,
        tasks: BTreeMap<String, Value>,
        writes: Vec<(String, Value)>,
        fail_create: bool,
        fail_get: Option<u16>,
        pages: usize,
        before_task_read: Option<Box<dyn FnOnce() + Send>>,
        skip_task_reads: usize,
    }

    impl Backend {
        fn handle(&mut self, method: &str, path: &str, body: Value) -> (u16, Value) {
            if method == "GET" {
                if let Some(response) = self.responses.get(path) {
                    return response.clone();
                }
            }
            if let Some(code) = self.fail_get {
                if method == "GET" {
                    return (code, json!({"err":"never expose this response"}));
                }
            }
            let route = path.split('?').next().unwrap();
            if method == "GET" && route == "/list/123" {
                return (200, metadata());
            }
            if method == "GET" && route == "/list/123/member" {
                return (
                    200,
                    json!({"members":[{"id":7,"username":"Sam"},{"id":8,"username":"Alex"}]}),
                );
            }
            if method == "GET" && route == "/list/123/task" {
                assert!(path.contains("include_closed=true"));
                assert!(path.contains("subtasks=true"));
                assert!(path.contains("include_markdown_description=true"));
                let page: usize = path.rsplit("page=").next().unwrap().parse().unwrap();
                self.pages += 1;
                let tasks: Vec<_> = self
                    .tasks
                    .values()
                    .skip(page * 2)
                    .take(2)
                    .cloned()
                    .collect();
                return (
                    200,
                    json!({"tasks":tasks,"last_page":(page+1)*2 >= self.tasks.len()}),
                );
            }
            if method == "POST" && route == "/list/123/task" {
                self.writes.push((method.into(), body.clone()));
                let id = format!("new{}", self.tasks.len());
                let mut task = remote(&id, body["name"].as_str().unwrap());
                update(&mut task, &body);
                self.tasks.insert(id, task.clone());
                return if self.fail_create {
                    (500, json!({}))
                } else {
                    (200, json!({"id": task["id"]}))
                };
            }
            if let Some(id) = route.strip_prefix("/task/") {
                if method == "GET" {
                    if self.skip_task_reads > 0 {
                        self.skip_task_reads -= 1;
                    } else if let Some(edit) = self.before_task_read.take() {
                        edit();
                    }
                    return self
                        .tasks
                        .get(id)
                        .cloned()
                        .map_or((404, json!({})), |t| (200, t));
                }
                if method == "PUT" {
                    self.writes.push((method.into(), body.clone()));
                    if let Some(task) = self.tasks.get_mut(id) {
                        update(task, &body);
                        return (200, task.clone());
                    }
                }
            }
            (404, json!({}))
        }
    }

    fn update(task: &mut Value, body: &Value) {
        for field in [
            "name",
            "due_date",
            "due_date_time",
            "start_date",
            "start_date_time",
        ] {
            if let Some(value) = body.get(field) {
                task[field] = value.clone();
            }
        }
        if let Some(notes) = body.get("markdown_content") {
            task["markdown_description"] = notes.clone();
        }
        if let Some(priority) = body.get("priority") {
            task["priority"] = if priority.is_null() {
                Value::Null
            } else {
                json!({"id":priority.to_string()})
            };
        }
        if let Some(assignees) = body.get("assignees") {
            let mut ids: Vec<i64> = task
                .get("assignees")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|a| a["id"].as_i64())
                .collect();
            if let Some(values) = assignees.as_array() {
                ids = values.iter().filter_map(Value::as_i64).collect();
            } else {
                let remove: Vec<_> = assignees["rem"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_i64)
                    .collect();
                ids.retain(|id| !remove.contains(id));
                ids.extend(
                    assignees["add"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(Value::as_i64),
                );
            }
            ids.sort_unstable();
            ids.dedup();
            task["assignees"] = json!(ids
                .into_iter()
                .map(|id| json!({"id":id,"username":if id==7 {"Sam"} else {"Alex"}}))
                .collect::<Vec<_>>());
        }
        if let Some(status) = body.get("status") {
            task["status"] =
                json!({"status":status,"type":if status == "closed" {"closed"} else {"custom"}});
            task["date_closed"] = if status == "closed" {
                json!("1700000000000")
            } else {
                Value::Null
            };
        }
    }

    struct Mock {
        state: Arc<Mutex<Backend>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
        api: Api,
    }

    impl Mock {
        fn new(tasks: Vec<Value>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let state = Arc::new(Mutex::new(Backend::default()));
            for task in tasks {
                state
                    .lock()
                    .unwrap()
                    .tasks
                    .insert(task["id"].as_str().unwrap().into(), task);
            }
            let stop = Arc::new(AtomicBool::new(false));
            let worker_state = state.clone();
            let worker_stop = stop.clone();
            let thread = thread::spawn(move || {
                while !worker_stop.load(Ordering::Relaxed) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(error) => panic!("{error}"),
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    let end = loop {
                        let mut buf = [0; 4096];
                        let n = stream.read(&mut buf).unwrap();
                        assert_ne!(n, 0);
                        request.extend_from_slice(&buf[..n]);
                        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                            break end + 4;
                        }
                    };
                    let headers = String::from_utf8_lossy(&request[..end]).to_string();
                    assert!(headers
                        .to_ascii_lowercase()
                        .contains("authorization: pk_test"));
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    while request.len() < end + length {
                        let mut buf = [0; 4096];
                        let n = stream.read(&mut buf).unwrap();
                        assert_ne!(n, 0);
                        request.extend_from_slice(&buf[..n]);
                    }
                    let mut first = headers.lines().next().unwrap().split_whitespace();
                    let method = first.next().unwrap();
                    let path = first.next().unwrap();
                    let body = if length == 0 {
                        Value::Null
                    } else {
                        serde_json::from_slice(&request[end..end + length]).unwrap()
                    };
                    let (status, body) = worker_state.lock().unwrap().handle(method, path, body);
                    let body = body.to_string();
                    write!(stream, "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            });
            let mut api = Api::new("pk_test").unwrap();
            api.base = format!("http://{address}");
            Self {
                state,
                stop,
                thread: Some(thread),
                api,
            }
        }
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let result = self.thread.take().unwrap().join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }

    fn setup() -> (tempfile::TempDir, Store, Link) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("tasks.sqlite3")).unwrap();
        let project = store.add_project("Work").unwrap();
        let link = Link {
            project_id: project.id,
            list_id: "123".into(),
            open_status: "to do".into(),
            working_status: "in progress".into(),
            done_status: "closed".into(),
            timezone: chrono_tz::Europe::Stockholm,
        };
        store.conn.execute(
                        "INSERT INTO clickup_projects(project_id,list_id,open_status,working_status,done_status,timezone) VALUES (?1,?2,?3,?4,?5,?6)",
                        params![project.id,link.list_id,link.open_status,link.working_status,link.done_status,link.timezone.to_string()]).unwrap();
        (dir, store, link)
    }

    fn local_id(store: &Store, remote: &str) -> i64 {
        store
            .conn
            .query_row(
                "SELECT local_id FROM clickup_tasks WHERE remote_id = ?1",
                [remote],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn clean(report: &Report) {
        assert!(
            report.issues.is_empty(),
            "{}",
            serde_json::to_string(&report.issues).unwrap()
        );
    }

    #[test]
    fn two_way_sync_paginates_creates_pulls_pushes_and_is_idempotent() {
        let (_dir, store, link) = setup();
        let mut closed = remote("r3", "Closed");
        closed["status"] = json!({"status":"closed","type":"closed"});
        closed["date_closed"] = json!("1700000000000");
        let mock = Mock::new(vec![remote("r1", "First"), remote("r2", "Second"), closed]);
        let local = store
            .add_task(
                "Local",
                "Local notes",
                Some(link.project_id),
                Priority::Low,
                Some("20260601"),
            )
            .unwrap();
        store.set_working(local.id, true).unwrap();
        let report = sync(&store, &mock.api, &link, None).unwrap();
        clean(&report);
        assert_eq!((report.imported, report.created), (3, 1));
        assert_eq!(mock.state.lock().unwrap().pages, 2);
        let closed = store.task(local_id(&store, "r3")).unwrap();
        assert!(closed.done);
        assert_eq!(
            closed.completed_at.as_deref(),
            Some("2023-11-14T22:13:20.000Z")
        );
        let created: Value = mock
            .state
            .lock()
            .unwrap()
            .tasks
            .values()
            .find(|t| t["name"] == "Local")
            .unwrap()
            .clone();
        assert_eq!(created["status"]["status"], "in progress");
        assert_eq!(created["due_date_time"], false);
        let date = Utc
            .timestamp_millis_opt(created["due_date"].as_i64().unwrap())
            .unwrap()
            .with_timezone(&link.timezone)
            .format("%Y-%m-%d")
            .to_string();
        assert_eq!(date, "2026-06-01");
        clean(&sync(&store, &mock.api, &link, None).unwrap());
        assert_eq!(mock.state.lock().unwrap().writes.len(), 1);
        let id = local_id(&store, "r1");
        store
            .edit_task(
                id,
                TaskEdit {
                    title: Some("Local edit".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let report = sync(&store, &mock.api, &link, None).unwrap();
        clean(&report);
        assert_eq!(report.pushed, 1);
        {
            let backend = mock.state.lock().unwrap();
            assert_eq!(backend.tasks["r1"]["name"], "Local edit");
            // A title edit must not flatten ClickUp urgent priority to high.
            assert_eq!(backend.tasks["r1"]["priority"]["id"], "1");
            assert_eq!(
                backend.writes.last().unwrap().1,
                json!({"name":"Local edit"})
            );
        }
        for notes in ["Migration of the deprecated vctlog platform", ""] {
            store
                .edit_task(
                    id,
                    TaskEdit {
                        notes: Some(notes.into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            let report = sync(&store, &mock.api, &link, None).unwrap();
            clean(&report);
            assert_eq!(report.pushed, 1);
            let backend = mock.state.lock().unwrap();
            assert_eq!(backend.tasks["r1"]["markdown_description"], notes);
            assert_eq!(
                backend.writes.last().unwrap().1,
                json!({"markdown_content":notes})
            );
        }
        mock.state.lock().unwrap().tasks.get_mut("r2").unwrap()["markdown_description"] =
            json!("Remote edit");
        let report = sync(&store, &mock.api, &link, None).unwrap();
        clean(&report);
        assert_eq!(report.pulled, 1);
        assert_eq!(
            store.task(local_id(&store, "r2")).unwrap().notes,
            "Remote edit"
        );
    }

    #[test]
    fn divergent_edits_require_explicit_resolution_and_deletions_never_propagate() {
        let (_dir, store, link) = setup();
        let mock = Mock::new(vec![remote("r1", "First"), remote("r2", "Second")]);
        clean(&sync(&store, &mock.api, &link, None).unwrap());
        let id = local_id(&store, "r1");
        store
            .edit_task(
                id,
                TaskEdit {
                    title: Some("Local wins".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        mock.state.lock().unwrap().tasks.get_mut("r1").unwrap()["name"] = json!("Remote wins");
        let report = sync(&store, &mock.api, &link, None).unwrap();
        assert_eq!(report.issues.len(), 1);
        assert!(report.issues[0].message.contains("Conflict"));
        assert!(mock.state.lock().unwrap().writes.is_empty());
        assert_eq!(store.task(id).unwrap().title, "Local wins");
        clean(&sync(&store, &mock.api, &link, Some((id, Choice::Local))).unwrap());
        assert_eq!(mock.state.lock().unwrap().tasks["r1"]["name"], "Local wins");
        store
            .edit_task(
                id,
                TaskEdit {
                    title: Some("Another local".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        mock.state.lock().unwrap().tasks.get_mut("r1").unwrap()["name"] = json!("Another remote");
        clean(&sync(&store, &mock.api, &link, Some((id, Choice::Remote))).unwrap());
        assert_eq!(store.task(id).unwrap().title, "Another remote");
        store.delete_task(id).unwrap();
        clean(&sync(&store, &mock.api, &link, None).unwrap());
        assert!(mock.state.lock().unwrap().tasks.contains_key("r1"));
        assert!(store.task(id).is_err());
        let other = local_id(&store, "r2");
        mock.state.lock().unwrap().tasks.remove("r2");
        let report = sync(&store, &mock.api, &link, None).unwrap();
        assert_eq!(report.issues.len(), 1);
        assert!(report.issues[0].message.contains("404"));
        assert!(store.task(other).is_ok());
        assert_eq!(mock.state.lock().unwrap().tasks.len(), 1);
    }

    #[test]
    fn uncertain_create_is_not_retried_or_reimported_and_can_be_recovered() {
        let (_dir, store, link) = setup();
        let mock = Mock::new(vec![]);
        let task = store
            .add_task("New", "", Some(link.project_id), Priority::Normal, None)
            .unwrap();
        mock.state.lock().unwrap().fail_create = true;
        let report = sync(&store, &mock.api, &link, None).unwrap();
        assert_eq!(report.issues.len(), 1);
        assert!(mappings(&store, link.project_id).unwrap()[0].pending);
        let report = sync(&store, &mock.api, &link, None).unwrap();
        assert_eq!(report.imported, 0);
        assert_eq!(report.created, 0);
        assert_eq!(mock.state.lock().unwrap().writes.len(), 1);
        assert_eq!(store.tasks(Scope::All, Status::All, "").unwrap().len(), 1);
        let remote_id = mock
            .state
            .lock()
            .unwrap()
            .tasks
            .keys()
            .next()
            .unwrap()
            .clone();
        // Local edits during failed requests must survive recovery too.
        store
            .edit_task(
                task.id,
                TaskEdit {
                    notes: Some("Keep this".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        recover(&store, &mock.api, task.id, &remote_id).unwrap();
        assert!(!mappings(&store, link.project_id).unwrap()[0].pending);
        let report = sync(&store, &mock.api, &link, None).unwrap();
        assert!(report.issues[0].message.contains("Conflict"));
        clean(&sync(&store, &mock.api, &link, Some((task.id, Choice::Local))).unwrap());
        assert_eq!(
            mock.state.lock().unwrap().tasks[&remote_id]["markdown_description"],
            "Keep this"
        );
        assert_eq!(mock.state.lock().unwrap().tasks.len(), 1);
    }

    #[test]
    fn status_validation_and_network_failures_leave_local_data_untouched() {
        let (_dir, store, link) = setup();
        let mock = Mock::new(vec![remote("r1", "First")]);
        let task = store
            .add_task(
                "Local only",
                "",
                Some(link.project_id),
                Priority::Normal,
                None,
            )
            .unwrap();
        for status in [401, 403, 429, 500] {
            mock.api.rate_limited.set(false);
            mock.state.lock().unwrap().fail_get = Some(status);
            let error = sync(&store, &mock.api, &link, None)
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains(&status.to_string()));
            assert!(!error.contains("never expose"));
            assert_eq!(store.task(task.id).unwrap().title, "Local only");
            assert!(mappings(&store, link.project_id).unwrap().is_empty());
        }
        assert!(mock.state.lock().unwrap().writes.is_empty());
        let mut invalid = link.clone();
        invalid.working_status = "closed".into();
        assert!(validate_statuses(&metadata(), &invalid).is_err());
        invalid.working_status = "missing".into();
        assert!(validate_statuses(&metadata(), &invalid).is_err());
    }

    #[test]
    fn date_timezone_conversion_and_partial_updates_preserve_remote_precision() {
        let (_dir, _store, link) = setup();
        let mut task = remote("r1", "Test");
        let midnight = link
            .timezone
            .with_ymd_and_hms(2026, 6, 1, 0, 30, 0)
            .unwrap();
        task["due_date"] = json!(midnight.timestamp_millis().to_string());
        task["due_date_time"] = json!(true);
        let snapshot = serde_json::from_value::<Remote>(task)
            .unwrap()
            .snapshot(&link)
            .unwrap();
        assert_eq!(snapshot.state.deadline.as_deref(), Some("2026-06-01"));
        let mut edited = snapshot.state.clone();
        edited.notes = "Only notes".into();
        assert_eq!(
            payload(&edited, Some(&snapshot.state), &link).unwrap(),
            json!({"markdown_content":"Only notes"})
        );
        edited.deadline = None;
        let body = payload(&edited, Some(&snapshot.state), &link).unwrap();
        assert!(body.get("due_date").unwrap().is_null());
        assert_eq!(body["due_date_time"], false);
        for invalid in ["../other", "abc?token=bad", "https://clickup.com/123", ""] {
            assert!(identifier(invalid).is_err());
        }
    }

    #[test]
    fn discovery_finds_folderless_and_nested_lists_deduplicates_and_reports_denied_spaces() {
        let mock = Mock::new(vec![]);
        {
            let mut state = mock.state.lock().unwrap();
            for (path, status, value) in [
                (
                    "/team",
                    200,
                    json!({"teams":[{"id":"w1","name":"Work"},{"id":2,"name":"Other"}]}),
                ),
                (
                    "/team/w1/space?archived=false",
                    200,
                    json!({"spaces":[{"id":"s1","name":"Space"}]}),
                ),
                ("/team/2/space?archived=false", 403, json!({})),
                (
                    "/space/s1/list?archived=false",
                    200,
                    json!({"lists":[{"id":"a","name":"Direct"}]}),
                ),
                (
                    "/space/s1/folder?archived=false",
                    200,
                    json!({"folders":[{"id":"f1","name":"Folder"},{"id":"f2","name":"Nested","parent_folder":"f1"},{"id":"f3","name":"Denied"}]}),
                ),
                (
                    "/folder/f1/list?archived=false",
                    200,
                    json!({"lists":[{"id":"b","name":"In folder"},{"id":"a","name":"Direct"}]}),
                ),
                (
                    "/folder/f2/list?archived=false",
                    200,
                    json!({"lists":[{"id":"c","name":"In nested folder"}]}),
                ),
                ("/folder/f3/list?archived=false", 403, json!({})),
            ] {
                state.responses.insert(path.into(), (status, value));
            }
        }
        let discovery = discover(&mock.api).unwrap();
        assert_eq!(
            discovery
                .lists
                .iter()
                .map(|l| l.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(discovery.lists[0].folder, None);
        assert_eq!(discovery.lists[1].folder.as_deref(), Some("Folder"));
        assert_eq!(discovery.lists[2].folder.as_deref(), Some("Nested"));
        assert_eq!(discovery.lists[0].workspace, "Work");
        assert_eq!(discovery.warnings.len(), 2);
        assert!(mock.state.lock().unwrap().writes.is_empty());
    }

    #[test]
    fn discovery_empty_and_root_failure_are_distinct_and_selections_are_validated() {
        let mock = Mock::new(vec![]);
        mock.state
            .lock()
            .unwrap()
            .responses
            .insert("/team".into(), (200, json!({"teams":[]})));
        assert!(discover(&mock.api).unwrap().lists.is_empty());
        mock.state
            .lock()
            .unwrap()
            .responses
            .insert("/team".into(), (401, json!({})));
        assert!(discover(&mock.api).is_err());
        assert_eq!(selections("3, 1,3", 3).unwrap(), [2, 0]);
        for invalid in ["0", "4", "1,0", "abc", "1,", "-1", ""] {
            assert!(selections(invalid, 3).is_err());
        }
    }

    #[test]
    fn exact_priorities_dates_and_assignees_sync_in_both_directions() {
        let (_dir, store, link) = setup();
        let mut initial = remote("r1", "Assigned");
        initial["start_date"] = json!(link
            .timezone
            .with_ymd_and_hms(2026, 5, 1, 13, 30, 0)
            .unwrap()
            .timestamp_millis()
            .to_string());
        initial["start_date_time"] = json!(true);
        initial["assignees"] = json!([{"id":7,"username":"Sam"}]);
        let mock = Mock::new(vec![initial]);
        assert_eq!(sync(&store, &mock.api, &link, None).unwrap().imported, 1);
        let task = store.tasks(Scope::All, Status::All, "").unwrap().remove(0);
        assert_eq!(task.priority, Priority::Urgent);
        assert_eq!(task.start_date.as_deref(), Some("2026-05-01"));
        assert_eq!(task.assignees[0].label(), "Sam");
        store
            .edit_task(
                task.id,
                TaskEdit {
                    notes: Some("Only notes".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        sync(&store, &mock.api, &link, None).unwrap();
        assert_eq!(
            mock.state.lock().unwrap().tasks["r1"]["start_date_time"],
            true
        );
        for (priority, remote_id) in [
            (Priority::None, None),
            (Priority::Low, Some("4")),
            (Priority::Normal, Some("3")),
            (Priority::High, Some("2")),
            (Priority::Urgent, Some("1")),
        ] {
            store
                .edit_task(
                    task.id,
                    TaskEdit {
                        priority: Some(priority),
                        ..Default::default()
                    },
                )
                .unwrap();
            let report = sync(&store, &mock.api, &link, None).unwrap();
            assert!(report.issues.is_empty());
            assert_eq!(
                mock.state.lock().unwrap().tasks["r1"]["priority"]["id"].as_str(),
                remote_id
            );
        }
        store.assign(task.id, &[8]).unwrap();
        store
            .edit_task(
                task.id,
                TaskEdit {
                    start_date: Some(Some("20260502".into())),
                    ..Default::default()
                },
            )
            .unwrap();
        let report = sync(&store, &mock.api, &link, None).unwrap();
        assert_eq!(report.pushed, 1);
        {
            let state = mock.state.lock().unwrap();
            let body = &state.writes.last().unwrap().1;
            assert_eq!(body["assignees"], json!({"add":[8],"rem":[7]}));
            assert_eq!(body["start_date_time"], false);
        }
        {
            let mut state = mock.state.lock().unwrap();
            state.tasks.get_mut("r1").unwrap()["assignees"] =
                json!([{"id":7,"username":"Sam"},{"id":8,"username":"Alex"}]);
            state.tasks.get_mut("r1").unwrap()["priority"] = Value::Null;
            state.tasks.get_mut("r1").unwrap()["start_date"] = json!(link
                .timezone
                .with_ymd_and_hms(2026, 5, 3, 4, 0, 0)
                .unwrap()
                .timestamp_millis()
                .to_string());
        }
        assert_eq!(sync(&store, &mock.api, &link, None).unwrap().pulled, 1);
        let task = store.task(task.id).unwrap();
        assert_eq!(task.assignees.len(), 2);
        assert_eq!(task.priority, Priority::None);
        assert_eq!(task.start_date.as_deref(), Some("2026-05-03"));
        assert!(task.completed_at.is_none());
        store.assign(task.id, &[]).unwrap();
        store
            .edit_task(
                task.id,
                TaskEdit {
                    start_date: Some(None),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(sync(&store, &mock.api, &link, None).unwrap().pushed, 1);
        assert!(mock.state.lock().unwrap().tasks["r1"]["start_date"].is_null());
        assert!(mock.state.lock().unwrap().tasks["r1"]["assignees"]
            .as_array()
            .unwrap()
            .is_empty());

        let created = store
            .add_task(
                "Create assigned",
                "",
                Some(link.project_id),
                Priority::Urgent,
                None,
            )
            .unwrap();
        store.assign(created.id, &[7]).unwrap();
        store
            .edit_task(
                created.id,
                TaskEdit {
                    start_date: Some(Some("20260504".into())),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(sync(&store, &mock.api, &link, None).unwrap().created, 1);
        let state = mock.state.lock().unwrap();
        let body = &state
            .writes
            .iter()
            .find(|(method, _)| method == "POST")
            .unwrap()
            .1;
        assert_eq!(body["assignees"], json!([7]));
        assert_eq!(body["priority"], 1);
        assert_eq!(body["start_date_time"], false);
    }

    #[test]
    fn automatic_cycles_discover_new_lists_reuse_links_push_pull_and_report_conflicts() {
        let (_dir, store, link) = setup();
        let mock = Mock::new(vec![remote("r1", "Remote task")]);
        {
            let mut state = mock.state.lock().unwrap();
            for (path, value) in [
                ("/user", json!({"user":{"timezone":"Europe/Stockholm"}})),
                ("/team", json!({"teams":[{"id":"w1","name":"Work"}]})),
                ("/team/w1/space?archived=false", json!({"spaces":[{"id":"s1","name":"Engineering"}]})),
                ("/space/s1/list?archived=false", json!({"lists":[{"id":"123","name":"Existing"},{"id":"456","name":"New"},{"id":"789","name":"Custom"}]})),
                ("/space/s1/folder?archived=false", json!({"folders":[]})),
                ("/list/456", metadata()),
                ("/list/456/member", json!({"members":[]})),
                ("/list/456/task?include_closed=true&subtasks=true&include_markdown_description=true&page=0", json!({"tasks":[],"last_page":true})),
                ("/list/789", json!({"statuses":[{"status":"open","type":"open"},{"status":"backlog","type":"open"},{"status":"in progress","type":"custom"},{"status":"closed","type":"closed"}]})),
            ] { state.responses.insert(path.into(), (200, value)); }
        }
        let first = auto_cycle_with_api(&store, &mock.api, true).unwrap();
        assert_eq!(first.projects, 2);
        assert_eq!(first.changed, 1);
        assert_eq!(first.issues.len(), 1);
        assert!(first.issues[0].contains("press C"));
        assert_eq!(store.clickup_projects().unwrap()[0].workspace, "Work");
        assert_eq!(store.projects().unwrap().len(), 2);
        assert_eq!(
            auto_cycle_with_api(&store, &mock.api, true)
                .unwrap()
                .changed,
            0
        );
        assert_eq!(store.projects().unwrap().len(), 2);
        let task = store
            .tasks(Scope::Project(link.project_id), Status::All, "")
            .unwrap()
            .remove(0);
        store
            .edit_task(
                task.id,
                TaskEdit {
                    notes: Some("Automatic push".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            auto_cycle_with_api(&store, &mock.api, false)
                .unwrap()
                .changed,
            1
        );
        assert_eq!(
            mock.state.lock().unwrap().tasks["r1"]["markdown_description"],
            "Automatic push"
        );
        mock.state.lock().unwrap().tasks.get_mut("r1").unwrap()["name"] = json!("Automatic pull");
        assert_eq!(
            auto_cycle_with_api(&store, &mock.api, false)
                .unwrap()
                .changed,
            1
        );
        assert_eq!(store.task(task.id).unwrap().title, "Automatic pull");
        store
            .edit_task(
                task.id,
                TaskEdit {
                    title: Some("Local edit".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        mock.state.lock().unwrap().tasks.get_mut("r1").unwrap()["name"] =
            json!("Conflicting remote edit");
        assert!(!auto_cycle_with_api(&store, &mock.api, false)
            .unwrap()
            .issues
            .is_empty());
        assert_eq!(store.task(task.id).unwrap().title, "Local edit");
        store
            .conn
            .execute("UPDATE clickup_preferences SET enabled=0", [])
            .unwrap();
        mock.state
            .lock()
            .unwrap()
            .responses
            .insert("/user".into(), (401, json!({})));
        assert!(auto_cycle_with_api(&store, &mock.api, true)
            .unwrap()
            .issues
            .is_empty());
    }

    #[test]
    fn concurrent_local_edits_are_preserved_during_background_pull_and_push() {
        let (dir, store, link) = setup();
        let mock = Mock::new(vec![remote("r1", "Original")]);
        sync(&store, &mock.api, &link, None).unwrap();
        let task = store.tasks(Scope::All, Status::All, "").unwrap().remove(0);
        let database = dir.path().join("tasks.sqlite3");
        mock.state.lock().unwrap().tasks.get_mut("r1").unwrap()["name"] = json!("Remote edit");
        let path = database.clone();
        mock.state.lock().unwrap().before_task_read = Some(Box::new(move || {
            Store::open(&path)
                .unwrap()
                .edit_task(
                    task.id,
                    TaskEdit {
                        notes: Some("Concurrent local edit".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
        }));
        let report = sync(&store, &mock.api, &link, None).unwrap();
        assert_eq!(report.pulled, 0);
        assert!(report.issues[0].message.contains("Conflict"));
        assert_eq!(store.task(task.id).unwrap().notes, "Concurrent local edit");
        assert_eq!(store.task(task.id).unwrap().title, "Original");
        sync(&store, &mock.api, &link, Some((task.id, Choice::Local))).unwrap();
        store
            .edit_task(
                task.id,
                TaskEdit {
                    title: Some("Local push".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        mock.state.lock().unwrap().skip_task_reads = 1;
        mock.state.lock().unwrap().before_task_read = Some(Box::new(move || {
            Store::open(&database)
                .unwrap()
                .edit_task(
                    task.id,
                    TaskEdit {
                        notes: Some("Later local edit".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
        }));
        assert_eq!(sync(&store, &mock.api, &link, None).unwrap().pushed, 1);
        assert_eq!(store.task(task.id).unwrap().notes, "Later local edit");
        assert_eq!(sync(&store, &mock.api, &link, None).unwrap().pushed, 1);
        assert_eq!(
            mock.state.lock().unwrap().tasks["r1"]["markdown_description"],
            "Later local edit"
        );
    }

    #[test]
    fn disabled_background_worker_does_not_require_credentials() {
        let (dir, store, _link) = setup();
        store
            .conn
            .execute("UPDATE clickup_preferences SET enabled=0", [])
            .unwrap();
        let worker = AutoSync::start(&dir.path().join("tasks.sqlite3"));
        let report = worker
            .results
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        assert_eq!(report.projects, 0);
        worker.request(false);
        assert!(worker
            .results
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap()
            .issues
            .is_empty());
    }

    #[test]
    fn importing_a_linked_list_reuses_its_project_without_prompting_or_creating_duplicates() {
        let (_dir, store, link) = setup();
        let mock = Mock::new(vec![]);
        let list = DiscoveredList {
            id: link.list_id.clone(),
            name: "Remote Work".into(),
            workspace: "Workspace".into(),
            space: "Space".into(),
            folder: None,
        };
        let selected = import_link(&store, &mock.api, &list, chrono_tz::UTC).unwrap();
        assert_eq!(selected.project_id, link.project_id);
        assert_eq!(selected.timezone, link.timezone);
        assert_eq!(store.projects().unwrap().len(), 1);
        assert!(mock.state.lock().unwrap().writes.is_empty());
    }
}
