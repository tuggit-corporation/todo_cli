use anyhow::{bail, Context, Result};
use chrono::{Local, NaiveDate};
use clap::ValueEnum;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{fmt, path::Path, time::Duration};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    None,
    Low,
    #[default]
    Normal,
    High,
    Urgent,
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::Urgent => "urgent",
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Project {
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct ClickupProject {
    pub project_id: i64,
    pub workspace: String,
    pub space: String,
    pub list_name: String,
    pub folder: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Task {
    pub id: i64,
    pub title: String,
    pub notes: String,
    pub project_id: Option<i64>,
    pub project_name: Option<String>,
    pub priority: Priority,
    pub done: bool,
    pub in_progress: bool,
    pub deadline: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub start_date: Option<String>,
    pub assignees: Vec<Assignee>,
    pub clickup_id: Option<String>,
    pub clickup_project: bool,
    pub github_url: Option<String>,
    pub github_assigned: bool,
    pub github_project_url: Option<String>,
    pub github_read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Assignee {
    pub id: i64,
    #[serde(default)]
    pub username: Option<String>,
}

impl Assignee {
    pub fn label(&self) -> String {
        self.username.clone().unwrap_or_else(|| self.id.to_string())
    }
}

impl Task {
    pub fn marker(&self) -> &'static str {
        if self.done {
            "x"
        } else if self.in_progress {
            ">"
        } else {
            " "
        }
    }

    pub fn overdue_on(&self, today: NaiveDate) -> bool {
        !self.done
            && self
                .deadline
                .as_deref()
                .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                .is_some_and(|date| date < today)
    }

    pub fn overdue(&self) -> bool {
        self.overdue_on(Local::now().date_naive())
    }
}

fn normalize_deadline(value: &str) -> Result<String> {
    let normalized;
    let value = if value.len() == 8 && value.bytes().all(|b| b.is_ascii_digit()) {
        normalized = format!("{}-{}-{}", &value[..4], &value[4..6], &value[6..]);
        normalized.as_str()
    } else {
        value
    };
    if value.len() != 10
        || value.as_bytes()[4] != b'-'
        || value.as_bytes()[7] != b'-'
        || !value
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
        || NaiveDate::parse_from_str(value, "%Y-%m-%d").is_err()
        || value.starts_with("0000")
    {
        bail!("Deadline must be a valid date in YYYY-MM-DD or YYYYMMDD format (for example 2026-12-31 or 20261231)");
    }
    Ok(value.to_owned())
}

#[derive(Clone, Copy, Debug, Default)]
pub enum Scope {
    #[default]
    All,
    Personal,
    Project(i64),
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum Status {
    #[default]
    Open,
    /// Only tasks currently being worked on
    InProgress,
    Done,
    All,
}

#[derive(Default)]
pub struct TaskEdit {
    pub title: Option<String>,
    pub notes: Option<String>,
    pub priority: Option<Priority>,
    pub project: Option<Option<i64>>,
    pub deadline: Option<Option<String>>,
    pub start_date: Option<Option<String>>,
    pub assignees: Option<Vec<Assignee>>,
}

pub struct Store {
    pub(crate) conn: Connection,
}

fn nonempty(value: &str, label: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{label} cannot be empty");
    }
    if value.chars().any(|c| c.is_control()) {
        bail!("{label} cannot contain control characters");
    }
    Ok(value.to_owned())
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)
            .with_context(|| format!("Cannot open database {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > 14 {
            bail!("This database was created by a newer todo version; update todo first");
        }
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS projects (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               name TEXT NOT NULL UNIQUE COLLATE NOCASE
             );
             CREATE TABLE IF NOT EXISTS tasks (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               title TEXT NOT NULL,
               notes TEXT NOT NULL DEFAULT '',
               project_id INTEGER REFERENCES projects(id) ON DELETE SET NULL,
               priority TEXT NOT NULL DEFAULT 'normal' CHECK(priority IN ('low','normal','high')),
               done INTEGER NOT NULL DEFAULT 0 CHECK(done IN (0,1)),
               created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
               updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE INDEX IF NOT EXISTS tasks_project_status ON tasks(project_id, done);",
        )?;
        if version < 2 {
            tx.execute_batch(
                "ALTER TABLE tasks ADD COLUMN deadline TEXT;
                 ALTER TABLE tasks ADD COLUMN in_progress INTEGER NOT NULL DEFAULT 0
                   CHECK(in_progress IN (0,1) AND (in_progress = 0 OR done = 0));
                 PRAGMA user_version = 2;",
            )?;
        }
        if version < 3 {
            tx.execute_batch(
                "CREATE TABLE clickup_projects (
                   project_id INTEGER PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
                   list_id TEXT NOT NULL UNIQUE,
                   open_status TEXT NOT NULL, working_status TEXT NOT NULL, done_status TEXT NOT NULL,
                   timezone TEXT NOT NULL
                 );
                 CREATE TABLE clickup_tasks (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   local_id INTEGER UNIQUE REFERENCES tasks(id) ON DELETE SET NULL,
                   project_id INTEGER NOT NULL REFERENCES clickup_projects(project_id) ON DELETE CASCADE,
                   remote_id TEXT UNIQUE,
                   local_base TEXT,
                   remote_base TEXT,
                   pending INTEGER NOT NULL DEFAULT 0
                 );
                 PRAGMA user_version = 3;",
            )?;
        }
        if version < 4 {
            tx.execute_batch(
                "ALTER TABLE tasks ADD COLUMN completed_at TEXT;
                 PRAGMA user_version = 4;",
            )?;
        }
        if version < 5 {
            tx.execute_batch(
                "ALTER TABLE tasks ADD COLUMN start_date TEXT;
                 ALTER TABLE tasks ADD COLUMN assignees TEXT NOT NULL DEFAULT '[]';
                 ALTER TABLE tasks ADD COLUMN priority_extra TEXT CHECK(priority_extra IN ('urgent','none'));
                 ALTER TABLE clickup_projects ADD COLUMN workspace TEXT NOT NULL DEFAULT '';
                 ALTER TABLE clickup_projects ADD COLUMN space TEXT NOT NULL DEFAULT '';
                 ALTER TABLE clickup_projects ADD COLUMN folder TEXT;
                 ALTER TABLE clickup_projects ADD COLUMN list_name TEXT NOT NULL DEFAULT '';
                 ALTER TABLE clickup_projects ADD COLUMN members TEXT NOT NULL DEFAULT '[]';
                 CREATE TABLE clickup_preferences (
                   id INTEGER PRIMARY KEY CHECK(id=1), enabled INTEGER NOT NULL DEFAULT 1,
                   all_lists INTEGER NOT NULL DEFAULT 1, timezone TEXT
                 );
                 INSERT INTO clickup_preferences(id) VALUES (1);
                 UPDATE tasks SET priority_extra='urgent'
                   WHERE priority='high' AND id IN (
                     SELECT local_id FROM clickup_tasks
                     WHERE json_extract(remote_base,'$.priority.id')='1'
                       AND json_extract(local_base,'$.priority')='high'
                   );
                 UPDATE tasks SET priority_extra='none'
                   WHERE priority='normal' AND id IN (
                     SELECT local_id FROM clickup_tasks
                     WHERE json_extract(remote_base,'$.priority') IS NULL
                       AND json_extract(local_base,'$.priority')='normal'
                   );
                 UPDATE clickup_tasks SET
                   local_base=json_set(local_base,'$.priority',
                     CASE WHEN json_extract(remote_base,'$.priority.id')='1' THEN 'urgent' ELSE 'none' END)
                   WHERE (json_extract(remote_base,'$.priority.id')='1'
                           AND json_extract(local_base,'$.priority')='high')
                      OR (remote_base IS NOT NULL AND json_extract(remote_base,'$.priority') IS NULL
                           AND json_extract(local_base,'$.priority')='normal');
                 UPDATE clickup_tasks SET
                   remote_base=json_set(remote_base,'$.state.priority',
                     CASE WHEN json_extract(remote_base,'$.priority.id')='1' THEN 'urgent' ELSE 'none' END)
                   WHERE (json_extract(remote_base,'$.priority.id')='1'
                           AND json_extract(remote_base,'$.state.priority')='high')
                      OR (remote_base IS NOT NULL AND json_extract(remote_base,'$.priority') IS NULL
                           AND json_extract(remote_base,'$.state.priority')='normal');
                 PRAGMA user_version = 5;",
            )?;
        }
        if version < 6 {
            tx.execute_batch(
                "ALTER TABLE clickup_preferences ADD COLUMN user_id INTEGER;
                              PRAGMA user_version = 6;",
            )?;
        }
        if version < 7 {
            tx.execute_batch("CREATE TABLE github_issues (
                url TEXT PRIMARY KEY, repository TEXT NOT NULL, number INTEGER NOT NULL,
                local_id INTEGER UNIQUE REFERENCES tasks(id) ON DELETE SET NULL,
                base_done INTEGER NOT NULL, assigned INTEGER NOT NULL DEFAULT 1,
                base_assignees TEXT NOT NULL DEFAULT '[]'
            );
            CREATE TABLE github_members(repository TEXT PRIMARY KEY, members TEXT NOT NULL);
            CREATE TABLE github_preferences(id INTEGER PRIMARY KEY CHECK(id=1), enabled INTEGER NOT NULL DEFAULT 0);
            INSERT INTO github_preferences(id) VALUES(1);
            PRAGMA user_version=7;")?;
        }
        if version < 8 {
            tx.execute_batch("CREATE TABLE github_projects (
                project_id INTEGER PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
                remote_id TEXT NOT NULL UNIQUE, owner TEXT NOT NULL, title TEXT NOT NULL, url TEXT NOT NULL
            );
            CREATE TABLE github_project_items (
                remote_id TEXT PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES github_projects(project_id) ON DELETE CASCADE,
                local_id INTEGER REFERENCES tasks(id) ON DELETE SET NULL, kind TEXT NOT NULL, content_id TEXT NOT NULL,
                status TEXT, fields TEXT NOT NULL DEFAULT '', assigned INTEGER NOT NULL DEFAULT 0,
                active INTEGER NOT NULL DEFAULT 1
            );
            PRAGMA user_version=8;")?;
        }
        if version < 9 {
            tx.execute_batch(
                "CREATE TABLE github_sources (
                kind TEXT NOT NULL CHECK(kind IN ('repo','project')), remote_id TEXT NOT NULL,
                label TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 0,
                project_id INTEGER REFERENCES projects(id) ON DELETE SET NULL,
                PRIMARY KEY(kind,remote_id)
            );
            INSERT INTO github_sources(kind,remote_id,label,project_id)
                SELECT 'project',remote_id,owner || ' / ' || title,project_id FROM github_projects;
            INSERT INTO github_sources(kind,remote_id,label,project_id)
                SELECT 'repo',g.repository,g.repository,min(t.project_id)
                FROM github_issues g LEFT JOIN tasks t ON t.id=g.local_id GROUP BY g.repository;
            CREATE INDEX github_sources_project ON github_sources(project_id,enabled);
            CREATE INDEX github_project_items_local ON github_project_items(local_id,active,project_id);
            PRAGMA user_version=9;",
            )?;
        }
        if version < 10 {
            tx.execute_batch(
                "CREATE INDEX IF NOT EXISTS github_sources_project ON github_sources(project_id,enabled);
                 CREATE INDEX IF NOT EXISTS github_project_items_local ON github_project_items(local_id,active,project_id);
                 PRAGMA user_version=10;"
            )?;
        }
        if version < 11 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS github_members(
                   repository TEXT PRIMARY KEY, members TEXT NOT NULL
                 );
                 PRAGMA user_version=11;",
            )?;
        }
        if version < 12 {
            let has_baseline: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('github_issues') WHERE name='base_assignees')",
                [], |r| r.get(0),
            )?;
            if !has_baseline {
                tx.execute_batch(
                    "ALTER TABLE github_issues ADD COLUMN base_assignees TEXT NOT NULL DEFAULT '[]';
                     UPDATE github_issues SET base_assignees=COALESCE(
                       (SELECT assignees FROM tasks WHERE id=github_issues.local_id),'[]'
                     );"
                )?;
            }
            tx.execute_batch("PRAGMA user_version=12;")?;
        }
        if version < 13 {
            for (column, field) in [("base_title", "title"), ("base_body", "notes")] {
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('github_issues') WHERE name=?1)",
                    [column],
                    |r| r.get(0),
                )?;
                if !exists {
                    tx.execute_batch(&format!(
                        "ALTER TABLE github_issues ADD COLUMN {column} TEXT NOT NULL DEFAULT '';
                         UPDATE github_issues SET {column}=COALESCE(
                           (SELECT {field} FROM tasks WHERE id=github_issues.local_id),''
                         );"
                    ))?;
                }
            }
            tx.execute_batch("PRAGMA user_version=13;")?;
        }
        if version < 14 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS clickup_sources(
                   list_id TEXT PRIMARY KEY, label TEXT NOT NULL, location TEXT NOT NULL,
                   enabled INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT OR IGNORE INTO clickup_sources(list_id,label,location)
                   SELECT list_id,
                     workspace || ' / ' || space || ' / ' || COALESCE(folder || ' / ','') ||
                       CASE WHEN list_name='' THEN p.name ELSE list_name END,
                     json_object('id',list_id,'name',CASE WHEN list_name='' THEN p.name ELSE list_name END,
                       'workspace',workspace,'space',space,'folder',folder)
                   FROM clickup_projects c JOIN projects p ON p.id=c.project_id;
                 UPDATE clickup_preferences SET all_lists=0;
                 PRAGMA user_version=14;"
            )?;
        }
        tx.commit()?;
        Ok(Self { conn })
    }

    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?)
    }

    pub fn clickup_user_id(&self) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT user_id FROM clickup_preferences WHERE id=1",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn assigned_to_me(&self, tasks: &mut Vec<Task>) -> Result<()> {
        let id = self.clickup_user_id()?;
        if id.is_none() && !crate::github::enabled(self)? {
            bail!("Account identity is not cached yet; connect and sync ClickUp or GitHub first");
        }

        tasks.retain(|task| {
            task.github_assigned
                || ((task.clickup_project || task.clickup_id.is_some())
                    && task.assignees.iter().any(|a| Some(a.id) == id))
        });
        Ok(())
    }

    pub fn github_context(&self, id: i64, scope: Scope) -> Result<String> {
        let project = match scope {
            Scope::Project(id) => Some(id),
            _ => None,
        };
        let mut stmt = self.conn.prepare("SELECT p.owner,p.title,i.status,i.fields FROM github_project_items i JOIN github_projects p ON p.project_id=i.project_id JOIN github_sources s ON s.kind='project' AND s.remote_id=p.remote_id WHERE i.local_id=?1 AND i.active=1 AND s.enabled=1 AND (?2 IS NULL OR i.project_id=?2) ORDER BY p.owner,p.title")?;
        let lines = stmt.query_map(params![id, project], |r| {
            Ok(format!(
                "{} / {}: {} {}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?
                    .unwrap_or_else(|| "No project status".into()),
                r.get::<_, String>(3)?
            ))
        })?;
        Ok(lines.collect::<rusqlite::Result<Vec<_>>>()?.join("\n"))
    }

    pub fn github_buckets(&self, scope: Scope) -> Result<std::collections::HashMap<i64, String>> {
        let project = match scope {
            Scope::Project(id) => Some(id),
            _ => None,
        };
        let mut stmt = self.conn.prepare(
            "SELECT i.local_id,group_concat(COALESCE(i.status,'No status'),' / ')
             FROM github_project_items i JOIN github_projects p ON p.project_id=i.project_id
             JOIN github_sources s ON s.kind='project' AND s.remote_id=p.remote_id
             WHERE i.local_id IS NOT NULL AND i.active=1 AND s.enabled=1
               AND (?1 IS NULL OR NOT EXISTS(SELECT 1 FROM github_projects WHERE project_id=?1) OR i.project_id=?1)
             GROUP BY i.local_id"
        )?;
        let rows = stmt.query_map([project], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name FROM projects ORDER BY name COLLATE NOCASE")?;
        let rows = stmt.query_map([], |row| {
            Ok(Project {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn visible_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id,p.name FROM projects p
             WHERE (NOT EXISTS(SELECT 1 FROM github_sources WHERE project_id=p.id)
                OR EXISTS(SELECT 1 FROM github_sources WHERE project_id=p.id AND enabled=1))
               AND NOT EXISTS(SELECT 1 FROM clickup_projects c JOIN clickup_sources s ON s.list_id=c.list_id
                          WHERE c.project_id=p.id AND s.enabled=0)
             ORDER BY p.name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Project {
                id: r.get(0)?,
                name: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_project(&self, name: &str) -> Result<Project> {
        let name = nonempty(name, "Project name")?;
        self.conn
            .execute("INSERT INTO projects(name) VALUES (?1)", [&name])
            .context("Cannot create project (names must be unique)")?;
        Ok(Project {
            id: self.conn.last_insert_rowid(),
            name,
        })
    }

    pub fn rename_project(&self, id: i64, name: &str) -> Result<()> {
        let name = nonempty(name, "Project name")?;
        let changed = self
            .conn
            .execute(
                "UPDATE projects SET name = ?1 WHERE id = ?2",
                params![name, id],
            )
            .context("Cannot rename project (names must be unique)")?;
        if changed == 0 {
            bail!("Project {id} not found");
        }
        Ok(())
    }

    pub fn delete_project(&mut self, id: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE tasks SET project_id = NULL, priority_extra = NULL, assignees = '[]',
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE project_id = ?1",
            [id],
        )?;
        if tx.execute("DELETE FROM projects WHERE id = ?1", [id])? == 0 {
            bail!("Project {id} not found");
        }
        tx.commit()?;
        Ok(())
    }

    fn check_project(&self, project_id: Option<i64>) -> Result<()> {
        if let Some(id) = project_id {
            if !self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)",
                [id],
                |row| row.get::<_, bool>(0),
            )? {
                bail!("Project {id} not found");
            }
        }
        Ok(())
    }

    pub fn is_clickup_project(&self, project: Option<i64>) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM clickup_projects WHERE project_id = ?1)",
            [project],
            |row| row.get(0),
        )?)
    }

    pub fn clickup_projects(&self) -> Result<Vec<ClickupProject>> {
        let mut stmt = self
            .conn
            .prepare("SELECT project_id,workspace,space,list_name,folder FROM clickup_projects")?;
        let rows = stmt.query_map([], |r| {
            Ok(ClickupProject {
                project_id: r.get(0)?,
                workspace: r.get(1)?,
                space: r.get(2)?,
                list_name: r.get(3)?,
                folder: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn members(&self, project: i64) -> Result<Vec<Assignee>> {
        let raw: String = self
            .conn
            .query_row(
                "SELECT members FROM clickup_projects WHERE project_id=?1",
                [project],
                |r| r.get(0),
            )
            .context("Not a ClickUp project")?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn github_members(&self, id: i64) -> Result<Vec<Assignee>> {
        let raw: Option<String> = self.conn.query_row(
            "SELECT m.members FROM github_issues i LEFT JOIN github_members m ON m.repository=i.repository WHERE i.local_id=?1",
            [id], |r| r.get(0)).optional()?.flatten();
        Ok(serde_json::from_str(raw.as_deref().unwrap_or("[]"))?)
    }

    pub fn is_github_project(&self, project: i64) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM github_projects WHERE project_id=?1) OR EXISTS(SELECT 1 FROM github_issues g JOIN tasks t ON t.id=g.local_id WHERE t.project_id=?1)",
            [project], |r| r.get(0))?)
    }

    pub fn assign(&self, id: i64, ids: &[i64]) -> Result<Task> {
        let task = self.task(id)?;
        let members = if task.github_url.is_some() {
            self.github_members(id)?
        } else {
            self.members(
                task.project_id
                    .context("Assignees require a connected project")?,
            )?
        };
        let mut assignees = Vec::new();
        for id in ids {
            let member = members
                .iter()
                .chain(&task.assignees)
                .find(|m| m.id == *id)
                .with_context(|| {
                    format!(
                        "Member {id} is not available in this List; refresh ClickUp members first"
                    )
                })?;
            if !assignees.iter().any(|a: &Assignee| a.id == *id) {
                assignees.push(member.clone());
            }
        }
        self.edit_task(
            task.id,
            TaskEdit {
                assignees: Some(assignees),
                ..Default::default()
            },
        )
    }

    fn check_priority(&self, project: Option<i64>, priority: Priority) -> Result<()> {
        if matches!(priority, Priority::Urgent | Priority::None)
            && !self.is_clickup_project(project)?
        {
            bail!("Urgent/none priorities are available only in ClickUp projects");
        }
        Ok(())
    }

    pub fn add_task(
        &self,
        title: &str,
        notes: &str,
        project: Option<i64>,
        priority: Priority,
        deadline: Option<&str>,
    ) -> Result<Task> {
        let title = nonempty(title, "Task title")?;
        let deadline = deadline.map(normalize_deadline).transpose()?;
        self.check_project(project)?;
        self.check_priority(project, priority)?;
        self.conn.execute(
            "INSERT INTO tasks(title, notes, project_id, priority, deadline, priority_extra) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![title, notes, project, base_priority(priority).to_string(), deadline, extra_priority(priority)],
        )?;
        self.task(self.conn.last_insert_rowid())
    }

    pub fn edit_task(&self, id: i64, mut edit: TaskEdit) -> Result<Task> {
        if let Some(assignees) = &mut edit.assignees {
            assignees.sort_by_key(|a| a.id);
            assignees.dedup_by_key(|a| a.id);
        }
        let current = self.task(id)?;
        let destination = edit.project.unwrap_or(current.project_id);
        self.check_priority(destination, edit.priority.unwrap_or(current.priority))?;
        if edit.assignees.is_some()
            && !self.is_clickup_project(destination)?
            && current.github_url.is_none()
        {
            bail!("Assignees can only be changed for connected tasks");
        }
        if let Some(assignees) = &edit.assignees {
            let members = if current.github_url.is_some() {
                self.github_members(id)?
            } else {
                self.members(destination.unwrap())?
            };
            if let Some(invalid) = assignees.iter().find(|a| {
                !members.iter().any(|m| m.id == a.id)
                    && !current.assignees.iter().any(|m| m.id == a.id)
            }) {
                bail!("Member {} is not available in this List", invalid.id);
            }
        }
        let start_date = edit
            .start_date
            .as_ref()
            .map(|value| value.as_deref().map(normalize_deadline).transpose())
            .transpose()?;
        let deadline = edit
            .deadline
            .as_ref()
            .map(|value| value.as_deref().map(normalize_deadline).transpose())
            .transpose()?;
        let title = edit
            .title
            .as_deref()
            .map(|t| nonempty(t, "Task title"))
            .transpose()?;
        let project = edit.project.flatten();
        self.check_project(project)?;
        self.conn.execute(
            "UPDATE tasks SET title = COALESCE(?1, title), notes = COALESCE(?2, notes),
             project_id = CASE WHEN ?3 THEN ?4 ELSE project_id END,
             priority = COALESCE(?5, priority),
             priority_extra = CASE WHEN ?9 THEN ?10 ELSE priority_extra END,
             deadline = CASE WHEN ?6 THEN ?7 ELSE deadline END,
             start_date = CASE WHEN ?11 THEN ?12 ELSE start_date END,
             assignees = COALESCE(?13, assignees),
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?8",
            params![
                title,
                edit.notes,
                edit.project.is_some(),
                project,
                edit.priority.map(|p| base_priority(p).to_string()),
                deadline.is_some(),
                deadline.flatten(),
                id,
                edit.priority.is_some(),
                edit.priority.and_then(extra_priority),
                start_date.is_some(),
                start_date.flatten(),
                edit.assignees
                    .map(|a| serde_json::to_string(&a))
                    .transpose()?,
            ],
        )?;
        self.task(id)
    }

    pub fn set_done(&self, id: i64, done: bool) -> Result<Task> {
        if self.task(id)?.github_read_only {
            bail!("GitHub project draft/PR items are read-only; change their state in GitHub");
        }
        if self.conn.execute(
            "UPDATE tasks SET done = ?1, in_progress = 0,
             completed_at = CASE WHEN ?1 = 0 THEN NULL WHEN done = 0 THEN strftime('%Y-%m-%dT%H:%M:%fZ','now') ELSE completed_at END,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?2",
            params![done, id],
        )? == 0
        {
            bail!("Task {id} not found");
        }
        self.task(id)
    }

    pub fn set_working(&self, id: i64, working: bool) -> Result<Task> {
        if self.conn.execute(
            "UPDATE tasks SET in_progress = ?1, done = CASE WHEN ?1 THEN 0 ELSE done END,
             completed_at = CASE WHEN ?1 THEN NULL ELSE completed_at END,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?2",
            params![working, id],
        )? == 0
        {
            bail!("Task {id} not found");
        }
        self.task(id)
    }

    pub fn delete_task(&self, id: i64) -> Result<()> {
        if self.conn.execute("DELETE FROM tasks WHERE id = ?1", [id])? == 0 {
            bail!("Task {id} not found");
        }
        Ok(())
    }

    pub fn task(&self, id: i64) -> Result<Task> {
        self.conn
            .query_row(&format!("{TASK_SELECT} WHERE t.id = ?1"), [id], read_task)
            .optional()?
            .with_context(|| format!("Task {id} not found"))
    }

    pub fn tasks(&self, scope: Scope, status: Status, search: &str) -> Result<Vec<Task>> {
        let (personal, project) = match scope {
            Scope::All => (false, None),
            Scope::Personal => (true, None),
            Scope::Project(id) => {
                self.check_project(Some(id))?;
                (false, Some(id))
            }
        };
        let done = match status {
            Status::Open | Status::InProgress => Some(false),
            Status::Done => Some(true),
            Status::All => None,
        };
        let mut stmt = self.conn.prepare(&format!(
            "{TASK_SELECT}
             WHERE (?1 = 0 OR t.project_id IS NULL)
               AND (?2 IS NULL OR (t.project_id = ?2 AND NOT EXISTS(SELECT 1 FROM github_projects WHERE project_id=?2)) OR EXISTS(SELECT 1 FROM github_project_items g WHERE g.local_id=t.id AND g.project_id=?2 AND g.active=1))
               AND (?3 IS NULL OR t.done = ?3)
               AND (?4 = '' OR instr(lower(t.title || char(10) || t.notes), lower(?4)) > 0)
               AND (?5 = 0 OR t.in_progress = 1)
               AND NOT EXISTS(SELECT 1 FROM clickup_projects c JOIN clickup_sources s ON s.list_id=c.list_id
                              WHERE c.project_id=t.project_id AND s.enabled=0)
               AND NOT EXISTS(SELECT 1 FROM clickup_tasks m JOIN clickup_projects c ON c.project_id=m.project_id
                              JOIN clickup_sources s ON s.list_id=c.list_id WHERE m.local_id=t.id AND s.enabled=0)
               AND (?2 IS NULL OR NOT EXISTS(SELECT 1 FROM github_sources WHERE project_id=?2)
                    OR EXISTS(SELECT 1 FROM github_sources WHERE project_id=?2 AND enabled=1))
               AND (
                 (NOT EXISTS(SELECT 1 FROM github_issues WHERE local_id=t.id)
                  AND NOT EXISTS(SELECT 1 FROM github_project_items WHERE local_id=t.id))
                 OR EXISTS(SELECT 1 FROM github_issues g JOIN github_sources s ON s.kind='repo' AND s.remote_id=g.repository WHERE g.local_id=t.id AND s.enabled=1)
                 OR EXISTS(SELECT 1 FROM github_project_items i JOIN github_projects p ON p.project_id=i.project_id
                   JOIN github_sources s ON s.kind='project' AND s.remote_id=p.remote_id
                   WHERE i.local_id=t.id AND i.active=1 AND s.enabled=1)
               )
             ORDER BY t.done, t.deadline IS NULL, t.deadline,
                      CASE COALESCE(t.priority_extra,t.priority) WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 WHEN 'low' THEN 3 ELSE 4 END,
                      t.id DESC"
        ))?;
        let rows = stmt.query_map(
            params![
                personal,
                project,
                done,
                search,
                matches!(status, Status::InProgress)
            ],
            read_task,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn base_priority(priority: Priority) -> Priority {
    match priority {
        Priority::Urgent => Priority::High,
        Priority::None => Priority::Normal,
        p => p,
    }
}

fn extra_priority(priority: Priority) -> Option<&'static str> {
    match priority {
        Priority::Urgent => Some("urgent"),
        Priority::None => Some("none"),
        _ => None,
    }
}

const TASK_SELECT: &str = "SELECT t.id, t.title, t.notes, t.project_id, p.name, COALESCE(t.priority_extra,t.priority), t.done,
            t.created_at, t.updated_at, t.deadline, t.in_progress, t.completed_at, t.start_date, t.assignees,
            (SELECT remote_id FROM clickup_tasks WHERE local_id=t.id),
            EXISTS(SELECT 1 FROM clickup_projects WHERE project_id=t.project_id),
            (SELECT url FROM github_issues WHERE local_id=t.id),
            (COALESCE((SELECT assigned FROM github_issues WHERE local_id=t.id),0) OR EXISTS(SELECT 1 FROM github_project_items i JOIN github_projects p ON p.project_id=i.project_id JOIN github_sources s ON s.kind='project' AND s.remote_id=p.remote_id WHERE i.local_id=t.id AND i.assigned=1 AND i.active=1 AND s.enabled=1)),
            (SELECT p.url FROM github_project_items i JOIN github_projects p ON p.project_id=i.project_id WHERE i.local_id=t.id LIMIT 1),
            EXISTS(SELECT 1 FROM github_project_items WHERE local_id=t.id AND kind!='Issue')
            FROM tasks t LEFT JOIN projects p ON p.id = t.project_id";

fn read_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    let priority: String = row.get(5)?;
    Ok(Task {
        id: row.get(0)?,
        title: row.get(1)?,
        notes: row.get(2)?,
        project_id: row.get(3)?,
        project_name: row.get(4)?,
        priority: match priority.as_str() {
            "urgent" => Priority::Urgent,
            "none" => Priority::None,
            "high" => Priority::High,
            "low" => Priority::Low,
            _ => Priority::Normal,
        },
        done: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        deadline: row.get(9)?,
        in_progress: row.get(10)?,
        completed_at: row.get(11)?,
        start_date: row.get(12)?,
        assignees: serde_json::from_str(&row.get::<_, String>(13)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(13, rusqlite::types::Type::Text, Box::new(e))
        })?,
        clickup_id: row.get(14)?,
        clickup_project: row.get(15)?,
        github_url: row.get(16)?,
        github_assigned: row.get(17)?,
        github_project_url: row.get(18)?,
        github_read_only: row.get(19)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrating_v9_adds_missing_lookup_indexes_without_changing_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = Store::open(&path).unwrap();
        let task = store
            .add_task("Keep", "", None, Priority::Normal, None)
            .unwrap();
        store.conn.execute_batch(
            "DROP INDEX github_sources_project; DROP INDEX github_project_items_local; PRAGMA user_version=9;"
        ).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.task(task.id).unwrap().title, "Keep");
        for (query,index) in [
            ("EXPLAIN QUERY PLAN SELECT 1 FROM github_sources WHERE project_id=1 AND enabled=1","github_sources_project"),
            ("EXPLAIN QUERY PLAN SELECT 1 FROM github_project_items WHERE local_id=1 AND active=1","github_project_items_local")
        ] {
            let mut stmt = store.conn.prepare(query).unwrap();
            let details = stmt.query_map([], |r| r.get::<_,String>(3)).unwrap()
                .collect::<rusqlite::Result<Vec<_>>>().unwrap().join(" ");
            assert!(details.contains(index),"{details}");
        }
    }

    #[test]
    fn migrating_v10_repairs_missing_github_assignee_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = Store::open(&path).unwrap();
        let task = store
            .add_task("Issue", "Keep notes", None, Priority::Normal, None)
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO github_issues(url,repository,number,local_id,base_done)
             VALUES('https://github.com/acme/repo/issues/1','acme/repo',1,?1,0)",
                [task.id],
            )
            .unwrap();
        store.conn.execute(
            "INSERT INTO github_sources(kind,remote_id,label,enabled) VALUES('repo','acme/repo','acme/repo',1)", []
        ).unwrap();
        store
            .conn
            .execute_batch("DROP TABLE github_members; PRAGMA user_version=10;")
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert!(store.github_members(task.id).unwrap().is_empty());
        assert_eq!(store.task(task.id).unwrap().notes, "Keep notes");
        assert!(crate::github::sources::list(&store).unwrap()[0].enabled);
        store.conn.execute(
            "INSERT INTO github_members(repository,members) VALUES('acme/repo','[{\"id\":7,\"username\":\"sam\"}]')", []
        ).unwrap();
        assert_eq!(
            store.assign(task.id, &[7]).unwrap().assignees[0].label(),
            "sam"
        );
        assert!(store.assign(task.id, &[]).unwrap().assignees.is_empty());
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.github_members(task.id).unwrap().len(), 1);
    }

    #[test]
    fn migrating_v13_makes_existing_clickup_lists_opt_in_without_deleting_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = Store::open(&path).unwrap();
        let project = store.add_project("Old ClickUp List").unwrap();
        store.conn.execute("INSERT INTO clickup_projects(project_id,list_id,open_status,working_status,done_status,timezone,workspace,space,list_name) VALUES(?1,'123','open','working','done','UTC','Work','Space','List')",[project.id]).unwrap();
        let task = store
            .add_task("Keep", "Notes", Some(project.id), Priority::High, None)
            .unwrap();
        store
            .conn
            .execute_batch("DROP TABLE clickup_sources; PRAGMA user_version=13;")
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let sources = crate::clickup::sources(&store).unwrap();
        assert_eq!(sources[0].label, "Work / Space / List");
        assert!(!sources[0].enabled);
        assert!(store.visible_projects().unwrap().is_empty());
        assert!(store.tasks(Scope::All, Status::All, "").unwrap().is_empty());
        assert_eq!(store.task(task.id).unwrap().notes, "Notes");
        crate::clickup::select_source(&store, "123", true).unwrap();
        assert_eq!(
            store.tasks(Scope::All, Status::All, "").unwrap()[0].id,
            task.id
        );
        drop(store);
        assert!(crate::clickup::sources(&Store::open(&path).unwrap()).unwrap()[0].enabled);
    }

    #[test]
    fn migrating_v12_baselines_existing_text_without_resetting_sync_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = Store::open(&path).unwrap();
        let task = store
            .add_task(
                "Local snapshot",
                "Existing notes",
                None,
                Priority::Normal,
                None,
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO github_issues(url,repository,number,local_id,base_done)
             VALUES('https://github.com/acme/repo/issues/1','acme/repo',1,?1,0)",
                [task.id],
            )
            .unwrap();
        store
            .conn
            .execute_batch(
                "ALTER TABLE github_issues DROP COLUMN base_title;
             ALTER TABLE github_issues DROP COLUMN base_body;
             PRAGMA user_version=12;",
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let baseline: (String, String, bool) = store
            .conn
            .query_row(
                "SELECT base_title,base_body,base_done FROM github_issues WHERE local_id=?1",
                [task.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            baseline,
            ("Local snapshot".into(), "Existing notes".into(), false)
        );
        assert_eq!(store.task(task.id).unwrap().notes, "Existing notes");
    }

    #[test]
    fn migrating_legacy_github_assignments_preserves_a_safe_baseline_and_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = Store::open(&path).unwrap();
        let task = store
            .add_task("Issue", "", None, Priority::Normal, None)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE tasks SET assignees='[{\"id\":7,\"username\":\"sam\"}]' WHERE id=?1",
                [task.id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO github_issues(url,repository,number,local_id,base_done)
             VALUES('https://github.com/acme/repo/issues/1','acme/repo',1,?1,0),
                   ('https://github.com/acme/repo/issues/2','acme/repo',2,NULL,0)",
                [task.id],
            )
            .unwrap();
        store
            .conn
            .execute_batch(
                "ALTER TABLE github_issues DROP COLUMN base_assignees;
             DROP TABLE github_members;
             PRAGMA user_version=10;",
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let baseline: String = store
            .conn
            .query_row(
                "SELECT base_assignees FROM github_issues WHERE local_id=?1",
                [task.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<Assignee>>(&baseline).unwrap(),
            store.task(task.id).unwrap().assignees
        );
        assert!(store.github_members(task.id).unwrap().is_empty());
        let tombstone: (Option<i64>, String) = store
            .conn
            .query_row(
                "SELECT local_id,base_assignees FROM github_issues WHERE number=2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(tombstone, (None, "[]".into()));
        drop(store);
        let store = Store::open(&path).unwrap();
        let saved: String = store
            .conn
            .query_row(
                "SELECT base_assignees FROM github_issues WHERE local_id=?1",
                [task.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(baseline, saved);
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.sqlite3")).unwrap();
        (dir, store)
    }

    #[test]
    fn task_lifecycle_persists_and_filters() {
        let (dir, store) = store();
        let project = store.add_project("Planner").unwrap();
        let task = store
            .add_task(
                " Ship ",
                "release notes",
                Some(project.id),
                Priority::High,
                None,
            )
            .unwrap();
        assert_eq!(task.title, "Ship");
        store
            .add_task("Personal", "", None, Priority::Normal, None)
            .unwrap();
        assert_eq!(
            store
                .tasks(Scope::Personal, Status::Open, "")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .tasks(Scope::Project(project.id), Status::Open, "NOTES")
                .unwrap()
                .len(),
            1
        );
        store
            .edit_task(
                task.id,
                TaskEdit {
                    title: Some("Shipped".into()),
                    notes: Some(String::new()),
                    priority: Some(Priority::Low),
                    project: Some(None),
                    ..TaskEdit::default()
                },
            )
            .unwrap();
        store.set_done(task.id, true).unwrap();
        assert!(store
            .tasks(Scope::All, Status::Open, "Shipped")
            .unwrap()
            .is_empty());
        drop(store);
        let store = Store::open(&dir.path().join("test.sqlite3")).unwrap();
        let saved = store.task(task.id).unwrap();
        assert!(saved.done);
        assert_eq!(saved.project_id, None);
        assert_eq!(saved.priority, Priority::Low);
        assert!(saved.notes.is_empty());
        store.set_done(task.id, false).unwrap();
        store.delete_task(task.id).unwrap();
        assert!(store.task(task.id).is_err());
    }

    #[test]
    fn deleting_project_preserves_tasks_as_personal() {
        let (_dir, mut store) = store();
        let project = store.add_project("Work").unwrap();
        let task = store
            .add_task("Keep me", "", Some(project.id), Priority::Normal, None)
            .unwrap();
        store.delete_project(project.id).unwrap();
        assert_eq!(store.task(task.id).unwrap().project_id, None);
        assert_eq!(
            store.tasks(Scope::Personal, Status::All, "").unwrap().len(),
            1
        );
    }

    #[test]
    fn migrating_v4_restores_clickup_priorities_without_discarding_local_edits() {
        let (dir, store) = store();
        let project = store.add_project("ClickUp").unwrap();
        store.conn.execute("INSERT INTO clickup_projects(project_id,list_id,open_status,working_status,done_status,timezone) VALUES (?1,'123','to do','in progress','closed','UTC')", [project.id]).unwrap();
        let mut ids = Vec::new();
        for (index, priority, remote_priority) in [
            (0, Priority::High, serde_json::json!({"id":"1"})),
            (1, Priority::Normal, serde_json::json!({"id":"1"})),
            (2, Priority::Normal, serde_json::Value::Null),
        ] {
            let task = store
                .add_task(
                    &format!("Task {index}"),
                    "Unsynced notes",
                    Some(project.id),
                    priority,
                    None,
                )
                .unwrap();
            let base = serde_json::json!({"title":task.title,"notes":"Old notes","priority":if index==2 {"normal"} else {"high"},"deadline":null,"done":false,"in_progress":false});
            let remote = serde_json::json!({"state":base,"priority":remote_priority,"status":"to do","due_date":null,"due_date_time":false,"completed_at":null});
            store.conn.execute("INSERT INTO clickup_tasks(local_id,project_id,remote_id,local_base,remote_base,pending) VALUES (?1,?2,?3,?4,?5,0)",
                params![task.id, project.id, format!("r{index}"), base.to_string(), remote.to_string()]).unwrap();
            ids.push(task.id);
        }
        store
            .conn
            .execute_batch(
                "ALTER TABLE tasks DROP COLUMN start_date;
             ALTER TABLE tasks DROP COLUMN assignees;
             ALTER TABLE tasks DROP COLUMN priority_extra;
             ALTER TABLE clickup_projects DROP COLUMN workspace;
             ALTER TABLE clickup_projects DROP COLUMN space;
             ALTER TABLE clickup_projects DROP COLUMN folder;
             ALTER TABLE clickup_projects DROP COLUMN list_name;
             ALTER TABLE clickup_projects DROP COLUMN members;
             DROP TABLE clickup_sources;
             DROP TABLE clickup_preferences;
             DROP TABLE github_issues;
             DROP TABLE github_sources;
             DROP TABLE github_members;
             DROP TABLE github_project_items;
             DROP TABLE github_projects;
             DROP TABLE github_preferences;
             PRAGMA user_version=4;",
            )
            .unwrap();
        drop(store);
        let store = Store::open(&dir.path().join("test.sqlite3")).unwrap();
        for (id, expected) in
            ids.into_iter()
                .zip([Priority::Urgent, Priority::Normal, Priority::None])
        {
            let task = store.task(id).unwrap();
            assert_eq!(task.priority, expected);
            assert_eq!(task.notes, "Unsynced notes");
            let base: String = store
                .conn
                .query_row(
                    "SELECT local_base FROM clickup_tasks WHERE local_id=?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            let base: serde_json::Value = serde_json::from_str(&base).unwrap();
            assert_eq!(base["notes"], "Old notes");
            assert_eq!(
                base["priority"],
                if expected == Priority::None {
                    "none"
                } else {
                    "urgent"
                }
            );
        }
    }

    #[test]
    fn clickup_only_fields_are_rejected_for_local_tasks_and_unknown_members() {
        let (_dir, mut store) = store();
        assert!(store
            .add_task("Local urgent", "", None, Priority::Urgent, None)
            .is_err());
        assert!(store
            .add_task("Local unset", "", None, Priority::None, None)
            .is_err());
        let task = store
            .add_task("Local", "", None, Priority::High, None)
            .unwrap();
        assert!(store.assign(task.id, &[7]).is_err());
        assert!(store
            .edit_task(
                task.id,
                TaskEdit {
                    priority: Some(Priority::None),
                    ..Default::default()
                }
            )
            .is_err());
        let project = store.add_project("ClickUp").unwrap();
        store.conn.execute("INSERT INTO clickup_projects(project_id,list_id,open_status,working_status,done_status,timezone) VALUES (?1,'123','to do','in progress','closed','UTC')", [project.id]).unwrap();
        store
            .edit_task(
                task.id,
                TaskEdit {
                    project: Some(Some(project.id)),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(store
            .edit_task(
                task.id,
                TaskEdit {
                    assignees: Some(vec![Assignee {
                        id: 7,
                        username: None
                    }]),
                    ..Default::default()
                }
            )
            .is_err());
        assert!(store.task(task.id).unwrap().assignees.is_empty());
        store
            .edit_task(
                task.id,
                TaskEdit {
                    priority: Some(Priority::Urgent),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(store
            .edit_task(
                task.id,
                TaskEdit {
                    project: Some(None),
                    ..Default::default()
                }
            )
            .is_err());
        store
            .conn
            .execute(
                "UPDATE tasks SET assignees='[{\"id\":7,\"username\":\"Sam\"}]' WHERE id=?1",
                [task.id],
            )
            .unwrap();
        store.delete_project(project.id).unwrap();
        let personal = store.task(task.id).unwrap();
        assert_eq!(personal.priority, Priority::High);
        assert!(!personal.clickup_project);
        assert!(personal.assignees.is_empty());
    }

    #[test]
    fn invalid_changes_leave_data_intact() {
        let (_dir, store) = store();
        assert!(store.add_project(" ").is_err());
        assert!(store
            .add_task("\u{1b}bad", "", None, Priority::Normal, None)
            .is_err());
        store.add_project("Work").unwrap();
        assert!(store.add_project("work").is_err());
        assert!(store
            .add_task("x", "", Some(999), Priority::Normal, None)
            .is_err());
        let task = store
            .add_task("Original", "", None, Priority::Normal, None)
            .unwrap();
        assert!(store
            .edit_task(
                task.id,
                TaskEdit {
                    title: Some(" ".into()),
                    notes: None,
                    priority: None,
                    project: None,
                    ..TaskEdit::default()
                }
            )
            .is_err());
        assert_eq!(store.task(task.id).unwrap().title, "Original");
        assert!(store.set_done(999, true).is_err());
        assert!(store.delete_task(999).is_err());
        assert!(store.tasks(Scope::Project(999), Status::All, "").is_err());
    }

    #[test]
    fn tasks_sort_by_due_date_before_priority_with_undated_last() {
        let (_dir, store) = store();
        let undated = store
            .add_task("Undated high", "", None, Priority::High, None)
            .unwrap();
        let later = store
            .add_task("Later high", "", None, Priority::High, Some("20261201"))
            .unwrap();
        let earlier = store
            .add_task("Earlier low", "", None, Priority::Low, Some("20260601"))
            .unwrap();
        assert_eq!(
            store
                .tasks(Scope::All, Status::Open, "")
                .unwrap()
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>(),
            [earlier.id, later.id, undated.id]
        );
    }

    #[test]
    fn compact_deadlines_normalize_and_validate_calendar_dates() {
        for (input, expected) in [
            ("20260601", "2026-06-01"),
            ("20280229", "2028-02-29"),
            ("2026-06-01", "2026-06-01"),
        ] {
            assert_eq!(normalize_deadline(input).unwrap(), expected);
        }
        for invalid in [
            "20260229",
            "20261301",
            "20260600",
            "00000601",
            "2026601",
            "202606011",
            "2026abcd",
            "２０２６０６０１",
        ] {
            assert!(normalize_deadline(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn deadlines_validate_persist_and_respect_completion() {
        let (dir, store) = store();
        let today = NaiveDate::from_ymd_opt(2026, 10, 29).unwrap();
        let task = store
            .add_task("Due", "", None, Priority::Normal, Some("2026-10-28"))
            .unwrap();
        assert!(task.overdue_on(today));
        store
            .edit_task(
                task.id,
                TaskEdit {
                    title: Some("Renamed".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        for invalid in [
            "2026-02-29",
            "2026-13-01",
            "2026-1-01",
            "2026-01-1",
            "0000-01-01",
            "tomorrow",
            "2026-10-29\n",
            "2026-10-29T12:00",
        ] {
            assert!(
                store
                    .edit_task(
                        task.id,
                        TaskEdit {
                            deadline: Some(Some(invalid.into())),
                            ..Default::default()
                        }
                    )
                    .is_err(),
                "{invalid}"
            );
            assert!(store
                .add_task("Invalid", "", None, Priority::Normal, Some(invalid))
                .is_err());
        }
        assert_eq!(
            store.task(task.id).unwrap().deadline.as_deref(),
            Some("2026-10-28")
        );
        drop(store);
        let store = Store::open(&dir.path().join("test.sqlite3")).unwrap();
        assert!(store.task(task.id).unwrap().overdue_on(today));
        assert!(!store.set_done(task.id, true).unwrap().overdue_on(today));
        store.set_done(task.id, false).unwrap();
        for date in ["2026-10-29", "2026-10-30", "2028-02-29"] {
            let task = store
                .edit_task(
                    task.id,
                    TaskEdit {
                        deadline: Some(Some(date.into())),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert!(!task.overdue_on(today));
        }
        let task = store
            .edit_task(
                task.id,
                TaskEdit {
                    deadline: Some(None),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(task.deadline.is_none());
        assert!(!task.overdue_on(today));
    }

    #[test]
    fn working_state_transitions_and_filters() {
        let (_dir, store) = store();
        let task = store
            .add_task("Work", "", None, Priority::Normal, None)
            .unwrap();
        assert!(!task.in_progress);
        assert!(store.set_working(task.id, true).unwrap().in_progress);
        assert!(store.set_working(task.id, true).unwrap().in_progress);
        assert_eq!(
            store
                .tasks(Scope::All, Status::InProgress, "")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(store.tasks(Scope::All, Status::Open, "").unwrap().len(), 1);
        let paused = store.set_working(task.id, false).unwrap();
        assert!(!paused.in_progress && !paused.done);
        assert!(store
            .tasks(Scope::All, Status::InProgress, "")
            .unwrap()
            .is_empty());
        store.set_working(task.id, true).unwrap();
        let done = store.set_done(task.id, true).unwrap();
        assert!(done.done && !done.in_progress);
        assert!(store
            .tasks(Scope::All, Status::InProgress, "")
            .unwrap()
            .is_empty());
        let started = store.set_working(task.id, true).unwrap();
        assert!(!started.done && started.in_progress);
        let reopened = store.set_done(task.id, false).unwrap();
        assert!(!reopened.done && !reopened.in_progress);
        assert!(store.set_working(999, true).is_err());
    }

    #[test]
    fn closing_records_time_without_rewriting_history_on_repeated_completion() {
        let (_dir, store) = store();
        let task = store
            .add_task("Close me", "", None, Priority::Normal, None)
            .unwrap();
        assert!(task.completed_at.is_none());
        let completed = store.set_done(task.id, true).unwrap();
        let closed_at = completed.completed_at.unwrap();
        assert!(chrono::DateTime::parse_from_rfc3339(&closed_at).is_ok());
        assert_eq!(
            store
                .set_done(task.id, true)
                .unwrap()
                .completed_at
                .as_deref(),
            Some(closed_at.as_str())
        );
        store
            .edit_task(
                task.id,
                TaskEdit {
                    notes: Some("More notes".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            store.task(task.id).unwrap().completed_at.as_deref(),
            Some(closed_at.as_str())
        );
        assert!(store
            .set_working(task.id, true)
            .unwrap()
            .completed_at
            .is_none());
        store.set_done(task.id, true).unwrap();
        assert!(store
            .set_working(task.id, false)
            .unwrap()
            .completed_at
            .is_some());
        assert!(store
            .set_done(task.id, false)
            .unwrap()
            .completed_at
            .is_none());
        store.set_done(task.id, true).unwrap();
        assert!(store.task(task.id).unwrap().completed_at.is_some());
    }

    #[test]
    fn migrates_v1_without_losing_existing_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite3");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE COLLATE NOCASE);
             CREATE TABLE tasks (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL,
               notes TEXT NOT NULL DEFAULT '', project_id INTEGER REFERENCES projects(id) ON DELETE SET NULL,
               priority TEXT NOT NULL DEFAULT 'normal' CHECK(priority IN ('low','normal','high')),
               done INTEGER NOT NULL DEFAULT 0 CHECK(done IN (0,1)),
               created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
               updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));
             INSERT INTO projects(name) VALUES ('Existing');
             INSERT INTO tasks(title, notes, project_id, priority, done) VALUES ('Keep', 'Notes', 1, 'high', 1);
             PRAGMA user_version = 1;"
        ).unwrap();
        drop(conn);
        for _ in 0..2 {
            let store = Store::open(&path).unwrap();
            let task = store.task(1).unwrap();
            assert_eq!(task.title, "Keep");
            assert_eq!(task.notes, "Notes");
            assert_eq!(task.project_name.as_deref(), Some("Existing"));
            assert_eq!(task.priority, Priority::High);
            assert!(task.done && !task.in_progress && task.deadline.is_none());
            assert!(task.completed_at.is_none());
            assert!(store.set_working(1, false).unwrap().done);
        }
    }
}
