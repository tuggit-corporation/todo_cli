use crate::store::{
    Assignee, ClickupProject, Priority, Project, Scope, Status, Store, Task, TaskEdit,
};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use std::{
    io,
    time::{Duration, Instant},
};

const SETTINGS: &[&str] = &[
    "GitHub sources (add / remove)",
    "Connect GitHub",
    "ClickUp sources (add / remove)",
    "Source / Workspace / Space filter",
    "Toggle assigned to me",
    "Cycle task status filter",
    "Sync issues",
    "ClickUp status mapping (advanced)",
    "Keyboard help",
];

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Projects,
    Tasks,
}

struct Form {
    id: Option<i64>,
    project_form: bool,
    fields: [String; 4],
    project: usize,
    priority: Priority,
    active: usize,
    original_task: Option<Box<Task>>,
    projects: Vec<Project>,
}

#[derive(Clone, PartialEq)]
enum SourceFilter {
    All,
    Local,
    ClickUp,
    GitHub,
    Workspace(String),
    Space { workspace: String, space: String },
}

impl SourceFilter {
    fn label(&self) -> String {
        match self {
            Self::All => "All sources".into(),
            Self::Local => "Local".into(),
            Self::ClickUp => "ClickUp".into(),
            Self::GitHub => "GitHub".into(),
            Self::Workspace(workspace) => format!("{workspace} / All Spaces"),
            Self::Space { workspace, space } => format!("{workspace} / {space}"),
        }
    }
}

enum Mode {
    Normal,
    GithubBuckets {
        options: Vec<crate::github::projects::Bucket>,
        cursor: usize,
        loading: bool,
    },
    Settings {
        cursor: usize,
    },
    GithubSources {
        cursor: usize,
    },
    Form(Form),
    Search(String),
    Delete {
        id: i64,
        project: bool,
    },
    Help,
    Filter {
        options: Vec<SourceFilter>,
        selected: usize,
    },
    Assignees {
        task: i64,
        members: Vec<Assignee>,
        selected: Vec<i64>,
        cursor: usize,
        original: Vec<Assignee>,
    },
    SyncIssues {
        cursor: usize,
    },
}

struct App {
    projects: Vec<Project>,
    tasks: Vec<Task>,
    project_index: usize,
    task_index: usize,
    project_state: ListState,
    task_state: ListState,
    focus: Focus,
    status: Status,
    search: String,
    message: String,
    mode: Mode,
    pending_g: bool,
    clickup_projects: Vec<ClickupProject>,
    source: SourceFilter,
    sync_status: String,
    sync_issues: Vec<String>,
    assigned_to_me: bool,
    github_details: String,
    github_sources: Vec<crate::github::sources::Source>,
    source_search: String,
    source_search_active: bool,
    clickup_source_menu: bool,
    refresh_clickup_sources: bool,
    sync_clickup_sources: bool,
    refresh_sources: bool,
    sync_sources: bool,
    pending_command: Option<KeyCode>,
    github_connected: bool,
    data_version: i64,
    bucket_request: Option<crate::github::Request>,
    bucket_generation: u64,
    current_buckets: std::collections::HashMap<i64, String>,
    moving_bucket: bool,
}

impl App {
    fn new(store: &Store) -> Result<Self> {
        let mut app = Self {
            projects: Vec::new(),
            tasks: Vec::new(),
            project_index: 0,
            task_index: 0,
            project_state: ListState::default(),
            task_state: ListState::default(),
            focus: Focus::Tasks,
            status: Status::Open,
            search: String::new(),
            message: "Local-first. Press ? for settings and help.".into(),
            mode: Mode::Normal,
            pending_g: false,
            clickup_projects: Vec::new(),
            source: SourceFilter::All,
            sync_status: "ClickUp: connecting...".into(),
            sync_issues: Vec::new(),
            assigned_to_me: false,
            github_details: String::new(),
            github_sources: Vec::new(),
            source_search: String::new(),
            source_search_active: false,
            clickup_source_menu: false,
            refresh_clickup_sources: false,
            sync_clickup_sources: false,
            refresh_sources: false,
            sync_sources: false,
            pending_command: None,
            github_connected: false,
            data_version: 0,
            bucket_request: None,
            bucket_generation: 0,
            current_buckets: Default::default(),
            moving_bucket: false,
        };
        app.refresh(store)?;
        Ok(app)
    }

    fn actions(&self) -> &'static str {
        if self.focus == Focus::Projects {
            return " Projects: j/k select  l tasks  a add task  P new project  e rename  d delete  ? settings";
        }
        match self.tasks.get(self.task_index) {
            Some(task) if task.github_read_only =>
                " GitHub Project: b move bucket  e local notes  S refresh | m mine  ? settings",
            Some(task) if task.github_url.is_some() =>
                " GitHub: b move bucket  Space close/reopen  A assignees  e edit issue  S sync | ? settings",
            Some(task) if task.clickup_project =>
                " ClickUp: Space done/reopen  A assignees  e edit  w work  p priority  S sync | m mine  ? settings",
            _ => " Local: a add  e edit  Space done/reopen  w work  p priority  d delete | m mine  ? settings",
        }
    }

    fn project_label(&self, id: Option<i64>) -> String {
        let project = self.projects.iter().find(|p| Some(p.id) == id).or_else(|| {
            if let Mode::Form(form) = &self.mode {
                form.projects.iter().find(|p| Some(p.id) == id)
            } else {
                None
            }
        });
        let Some(project) = project else {
            return "Personal".into();
        };
        let Some(link) = self
            .clickup_projects
            .iter()
            .find(|c| c.project_id == project.id)
        else {
            if project.name.starts_with("GitHub: ") {
                return format!("[GH] {}", project.name.trim_start_matches("GitHub: "));
            }
            if project.name.starts_with("GitHub Project: ") {
                return format!(
                    "[GH] {}",
                    project.name.trim_start_matches("GitHub Project: ")
                );
            }
            return project.name.clone();
        };
        let group = link
            .folder
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(&link.space);
        let label = if group.is_empty() {
            project.name.clone()
        } else if link.list_name.is_empty() || link.list_name.eq_ignore_ascii_case("list") {
            group.to_owned()
        } else {
            format!("{group} / {}", link.list_name)
        };
        format!("[CU] {label}")
    }

    fn source_options(&self) -> Vec<&crate::github::sources::Source> {
        let query = self.source_search.to_lowercase();
        self.github_sources
            .iter()
            .filter(|s| s.label.to_lowercase().contains(&query))
            .collect()
    }

    fn scope(&self) -> Scope {
        match self.project_index {
            0 => Scope::All,
            1 => Scope::Personal,
            i => self
                .projects
                .get(i - 2)
                .map_or(Scope::All, |p| Scope::Project(p.id)),
        }
    }

    fn sync_project(&self) -> Option<i64> {
        match self.scope() {
            Scope::Project(id) => Some(id),
            _ if self.focus == Focus::Tasks => {
                self.tasks.get(self.task_index).and_then(|t| t.project_id)
            }
            _ => None,
        }
    }

    fn refresh(&mut self, store: &Store) -> Result<()> {
        let data_version = store.data_version()?;
        self.github_connected = crate::github::enabled(store)?;
        let project_id = match self.scope() {
            Scope::Project(id) => Some(id),
            _ => None,
        };
        let task_id = self.tasks.get(self.task_index).map(|t| t.id);
        self.projects = store.visible_projects()?;
        self.github_sources = if self.clickup_source_menu {
            crate::clickup::sources(store)?
        } else {
            crate::github::sources::list(store)?
        };
        self.clickup_projects = store.clickup_projects()?;
        if let Some(id) = project_id {
            self.project_index = self
                .projects
                .iter()
                .position(|p| p.id == id)
                .map_or(0, |i| i + 2);
        }
        self.tasks = store.tasks(self.scope(), self.status, &self.search)?;
        self.current_buckets = store.github_buckets(self.scope())?;
        self.tasks.retain(|task| match &self.source {
            SourceFilter::All => true,
            SourceFilter::Local => {
                !task.clickup_project
                    && task.clickup_id.is_none()
                    && task.github_url.is_none()
                    && task.github_project_url.is_none()
            }
            SourceFilter::ClickUp => task.clickup_project || task.clickup_id.is_some(),
            SourceFilter::GitHub => task.github_url.is_some() || task.github_project_url.is_some(),
            SourceFilter::Workspace(workspace) => self
                .clickup_projects
                .iter()
                .any(|p| Some(p.project_id) == task.project_id && &p.workspace == workspace),
            SourceFilter::Space { workspace, space } => self.clickup_projects.iter().any(|p| {
                Some(p.project_id) == task.project_id
                    && &p.workspace == workspace
                    && &p.space == space
            }),
        });
        if self.assigned_to_me {
            if store.clickup_user_id()?.is_some() || crate::github::enabled(store)? {
                store.assigned_to_me(&mut self.tasks)?;
            } else {
                self.tasks.clear();
                self.message =
                    "Waiting for account identity; connect ClickUp or GitHub first.".into();
            }
        }
        self.task_index = task_id
            .and_then(|id| self.tasks.iter().position(|t| t.id == id))
            .unwrap_or(self.task_index)
            .min(self.tasks.len().saturating_sub(1));
        self.github_details = self
            .tasks
            .get(self.task_index)
            .map(|t| store.github_context(t.id, self.scope()))
            .transpose()?
            .unwrap_or_default();
        self.data_version = data_version;
        Ok(())
    }

    fn refresh_if_changed(&mut self, store: &Store) -> Result<()> {
        if self.data_version != store.data_version()? {
            self.refresh(store)?;
        }
        Ok(())
    }

    fn move_selection(&mut self, amount: isize, store: &Store) -> Result<()> {
        if self.focus == Focus::Projects {
            self.project_index = self
                .project_index
                .saturating_add_signed(amount)
                .min(self.projects.len() + 1);
            self.task_index = 0;
            self.tasks.clear();
            self.refresh(store)?;
        } else {
            self.task_index = self
                .task_index
                .saturating_add_signed(amount)
                .min(self.tasks.len().saturating_sub(1));
        }
        self.github_details = self
            .tasks
            .get(self.task_index)
            .map(|t| store.github_context(t.id, self.scope()))
            .transpose()?
            .unwrap_or_default();
        Ok(())
    }

    fn form(&mut self, edit: bool, project_form: bool) {
        let mut form = Form {
            id: None,
            project_form,
            fields: Default::default(),
            project: match self.scope() {
                Scope::Project(id) => self
                    .projects
                    .iter()
                    .position(|p| p.id == id)
                    .map_or(0, |i| i + 1),
                _ => 0,
            },
            priority: if matches!(self.scope(), Scope::Project(id) if self.clickup_projects.iter().any(|p| p.project_id == id))
            {
                Priority::None
            } else {
                Priority::Normal
            },
            active: 0,
            original_task: None,
            projects: self.projects.clone(),
        };
        if edit {
            if project_form {
                let Some(project) = self
                    .project_index
                    .checked_sub(2)
                    .and_then(|i| self.projects.get(i))
                else {
                    return;
                };
                form.id = Some(project.id);
                form.fields[0] = project.name.clone();
            } else {
                let Some(task) = self.tasks.get(self.task_index) else {
                    return;
                };
                if let (Some(id), Some(name)) = (task.project_id, &task.project_name) {
                    if !form.projects.iter().any(|p| p.id == id) {
                        form.projects.push(Project {
                            id,
                            name: name.clone(),
                        });
                    }
                }
                form.id = Some(task.id);
                form.fields = [
                    task.title.clone(),
                    task.notes.clone(),
                    task.deadline.clone().unwrap_or_default(),
                    task.start_date.clone().unwrap_or_default(),
                ];
                form.original_task = Some(Box::new(task.clone()));
                form.priority = task.priority;
                form.project = task
                    .project_id
                    .and_then(|id| form.projects.iter().position(|p| p.id == id))
                    .map_or(0, |i| i + 1);
            }
        }
        self.mode = Mode::Form(form);
    }

    fn save_form(&mut self, form: &Form, store: &Store) -> Result<()> {
        if form.project_form {
            if let Some(id) = form.id {
                store.rename_project(id, &form.fields[0])?;
            } else {
                let project = store.add_project(&form.fields[0])?;
                self.refresh(store)?;
                self.project_index = self
                    .projects
                    .iter()
                    .position(|p| p.id == project.id)
                    .unwrap()
                    + 2;
                self.tasks.clear();
                self.task_index = 0;
                self.focus = Focus::Tasks;
            }
        } else {
            let deadline = (!form.fields[2].is_empty()).then_some(form.fields[2].as_str());
            let project = form
                .project
                .checked_sub(1)
                .and_then(|i| form.projects.get(i))
                .map(|p| p.id);
            if let Some(id) = form.id {
                let tx = rusqlite::Transaction::new_unchecked(
                    &store.conn,
                    rusqlite::TransactionBehavior::Immediate,
                )?;
                if form.original_task.as_deref() != Some(&store.task(id)?) {
                    anyhow::bail!("Task changed during editing. Esc and reopen it to avoid overwriting newer changes");
                }
                store.edit_task(
                    id,
                    TaskEdit {
                        title: Some(form.fields[0].clone()),
                        notes: Some(form.fields[1].clone()),
                        priority: Some(form.priority),
                        project: Some(project),
                        deadline: Some(deadline.map(str::to_owned)),
                        start_date: Some(
                            (!form.fields[3].is_empty()).then(|| form.fields[3].clone()),
                        ),
                        ..TaskEdit::default()
                    },
                )?;
                tx.commit()?;
            } else {
                store.conn.execute_batch("BEGIN IMMEDIATE")?;
                let result = (|| -> Result<_> {
                    let task = store.add_task(
                        &form.fields[0],
                        &form.fields[1],
                        project,
                        form.priority,
                        deadline,
                    )?;
                    store.edit_task(
                        task.id,
                        TaskEdit {
                            start_date: Some(
                                (!form.fields[3].is_empty()).then(|| form.fields[3].clone()),
                            ),
                            ..Default::default()
                        },
                    )
                })();
                let task = match result {
                    Ok(task) => {
                        store.conn.execute_batch("COMMIT")?;
                        task
                    }
                    Err(error) => {
                        store.conn.execute_batch("ROLLBACK")?;
                        return Err(error);
                    }
                };
                self.refresh(store)?;
                self.task_index = self.tasks.iter().position(|t| t.id == task.id).unwrap_or(0);
            }
        }
        self.message = "Saved. Linked issue titles/notes, status and assignees sync automatically; GitHub priority/dates/work markers stay local.".into();
        self.refresh(store)
    }

    fn key(&mut self, key: KeyEvent, store: &mut Store) -> Result<bool> {
        if key.kind == KeyEventKind::Release {
            return Ok(false);
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(true);
        }
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        match mode {
            Mode::GithubBuckets {
                options,
                mut cursor,
                loading,
            } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Char('j') | KeyCode::Down => {
                    cursor = (cursor + 1).min(options.len().saturating_sub(1));
                    self.mode = Mode::GithubBuckets {
                        options,
                        cursor,
                        loading,
                    };
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    cursor = cursor.saturating_sub(1);
                    self.mode = Mode::GithubBuckets {
                        options,
                        cursor,
                        loading,
                    };
                }
                KeyCode::Enter if !loading => {
                    if let Some(bucket) = options.get(cursor) {
                        self.bucket_request =
                            Some(crate::github::Request::MoveBucket(bucket.clone()));
                        self.message = format!("Moving to {} on GitHub...", bucket.label);
                        self.moving_bucket = true;
                    }
                }
                _ => {
                    self.mode = Mode::GithubBuckets {
                        options,
                        cursor,
                        loading,
                    }
                }
            },
            Mode::Settings { mut cursor } => match key.code {
                KeyCode::Esc | KeyCode::Char('q' | '?') => {}
                KeyCode::Char('j') | KeyCode::Down => {
                    cursor = (cursor + 1).min(SETTINGS.len() - 1);
                    self.mode = Mode::Settings { cursor };
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.mode = Mode::Settings {
                        cursor: cursor.saturating_sub(1),
                    };
                }
                KeyCode::Enter => match cursor {
                    0 => {
                        self.clickup_source_menu = false;
                        self.github_sources = crate::github::sources::list(store)?;
                        self.refresh_sources = self.github_sources.is_empty();
                        self.source_search.clear();
                        self.source_search_active = false;
                        self.mode = Mode::GithubSources { cursor: 0 };
                    }
                    1 => {
                        self.github_connected = crate::github::enabled(store)?;
                        if self.github_connected {
                            self.message = "GitHub is already connected. Manage repositories and Projects under GitHub sources.".into();
                            self.mode = Mode::Settings { cursor };
                        } else {
                            self.pending_command = Some(KeyCode::Char('H'));
                        }
                    }
                    7 => self.pending_command = Some(KeyCode::Char('C')),
                    3 => {
                        self.key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE), store)?;
                    }
                    4 => {
                        self.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE), store)?;
                    }
                    5 => {
                        self.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), store)?;
                    }
                    6 => self.mode = Mode::SyncIssues { cursor: 0 },
                    2 => {
                        self.clickup_source_menu = true;
                        self.github_sources = crate::clickup::sources(store)?;
                        self.refresh_clickup_sources = self.github_sources.is_empty();
                        self.source_search.clear();
                        self.source_search_active = false;
                        self.mode = Mode::GithubSources { cursor: 0 };
                    }
                    _ => self.mode = Mode::Help,
                },
                _ => self.mode = Mode::Settings { cursor },
            },
            Mode::GithubSources { mut cursor } => {
                if self.source_search_active {
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter => self.source_search_active = false,
                        KeyCode::Backspace => {
                            self.source_search.pop();
                        }
                        KeyCode::Char(c) if !c.is_control() => self.source_search.push(c),
                        _ => {}
                    }
                    self.mode = Mode::GithubSources { cursor: 0 };
                    return Ok(false);
                }
                cursor = cursor.min(self.source_options().len().saturating_sub(1));
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Settings { cursor: 0 },
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.mode = Mode::GithubSources {
                            cursor: (cursor + 1).min(self.source_options().len().saturating_sub(1)),
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.mode = Mode::GithubSources {
                            cursor: cursor.saturating_sub(1),
                        }
                    }
                    KeyCode::Char('r') => {
                        if self.clickup_source_menu {
                            self.refresh_clickup_sources = true;
                        } else {
                            self.refresh_sources = true;
                        }
                        self.mode = Mode::GithubSources { cursor };
                    }
                    KeyCode::Char('/') => {
                        self.source_search_active = true;
                        self.mode = Mode::GithubSources { cursor };
                    }
                    KeyCode::Char(' ') | KeyCode::Enter => {
                        if let Some(source) = self.source_options().get(cursor) {
                            if self.clickup_source_menu {
                                crate::clickup::select_source(
                                    store,
                                    &source.remote_id,
                                    !source.enabled,
                                )?;
                                self.sync_clickup_sources = true;
                            } else {
                                crate::github::sources::set_enabled(
                                    store,
                                    &source.kind,
                                    &source.remote_id,
                                    !source.enabled,
                                )?;
                                self.sync_sources = true;
                            }
                            self.refresh(store)?;
                        }
                        self.mode = Mode::GithubSources { cursor };
                    }
                    _ => self.mode = Mode::GithubSources { cursor },
                }
            }
            Mode::SyncIssues { mut cursor } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Char('j') | KeyCode::Down => {
                    cursor = (cursor + 1).min(self.sync_issues.len().saturating_sub(1));
                    self.mode = Mode::SyncIssues { cursor };
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.mode = Mode::SyncIssues {
                        cursor: cursor.saturating_sub(1),
                    };
                }
                _ => self.mode = Mode::SyncIssues { cursor },
            },
            Mode::Filter {
                options,
                mut selected,
            } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('j') | KeyCode::Down => {
                    selected = (selected + 1).min(options.len() - 1);
                    self.mode = Mode::Filter { options, selected };
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    self.mode = Mode::Filter { options, selected };
                }
                KeyCode::Enter => {
                    self.source = options[selected].clone();
                    self.project_index = 0;
                    self.task_index = 0;
                    self.tasks.clear();
                    self.refresh(store)?;
                }
                _ => self.mode = Mode::Filter { options, selected },
            },
            Mode::Assignees {
                task,
                members,
                mut selected,
                mut cursor,
                original,
            } => {
                let mut close = false;
                match key.code {
                    KeyCode::Esc => close = true,
                    KeyCode::Char('j') | KeyCode::Down => {
                        cursor = (cursor + 1).min(members.len().saturating_sub(1))
                    }
                    KeyCode::Char('k') | KeyCode::Up => cursor = cursor.saturating_sub(1),
                    KeyCode::Char(' ') => {
                        if let Some(member) = members.get(cursor) {
                            if selected.contains(&member.id) {
                                selected.retain(|id| *id != member.id);
                            } else {
                                selected.push(member.id);
                            }
                        }
                    }
                    KeyCode::Enter => {
                        let tx = rusqlite::Transaction::new_unchecked(
                            &store.conn,
                            rusqlite::TransactionBehavior::Immediate,
                        )?;
                        let current = store.task(task)?;
                        if current.assignees != original {
                            self.message =
                                "Assignees changed remotely; Esc and reopen the picker.".into();
                        } else {
                            // Preserve current assignees not in the assignable-member list.
                            let mut values: Vec<_> = original
                                .iter()
                                .filter(|a| selected.contains(&a.id))
                                .cloned()
                                .collect();
                            for member in &members {
                                if selected.contains(&member.id)
                                    && !values.iter().any(|a| a.id == member.id)
                                {
                                    values.push(member.clone());
                                }
                            }
                            store.edit_task(
                                task,
                                TaskEdit {
                                    assignees: Some(values),
                                    ..Default::default()
                                },
                            )?;
                            tx.commit()?;
                            self.refresh(store)?;
                            self.message = "Assignees saved; sync queued.".into();
                            close = true;
                        }
                    }
                    _ => {}
                }
                if !close {
                    self.mode = Mode::Assignees {
                        task,
                        members,
                        selected,
                        cursor,
                        original,
                    };
                }
            }
            Mode::Help => {}
            Mode::Delete { id, project } => {
                if key.code == KeyCode::Char('y') {
                    if project {
                        store.delete_project(id)?;
                    } else {
                        store.delete_task(id)?;
                    }
                    self.message = if project {
                        "Project deleted; tasks moved to Personal."
                    } else {
                        "Task deleted."
                    }
                    .into();
                    self.refresh(store)?;
                } else if !matches!(key.code, KeyCode::Esc | KeyCode::Char('n')) {
                    self.mode = Mode::Delete { id, project };
                }
            }
            Mode::Search(mut text) => {
                match key.code {
                    KeyCode::Esc => return Ok(false),
                    KeyCode::Enter => {
                        self.search = text;
                        self.task_index = 0;
                        self.tasks.clear();
                        self.refresh(store)?;
                        return Ok(false);
                    }
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Char(c) if !c.is_control() => text.push(c),
                    _ => {}
                }
                self.mode = Mode::Search(text);
            }
            Mode::Form(mut form) => {
                let save = (key.code == KeyCode::Char('s')
                    && key.modifiers.contains(KeyModifiers::CONTROL))
                    || (form.project_form && key.code == KeyCode::Enter);
                if save {
                    if let Err(error) = self.save_form(&form, store) {
                        self.message = format!("{error:#}");
                        self.mode = Mode::Form(form);
                    }
                    return Ok(false);
                }
                let count = if form.project_form { 1 } else { 6 };
                let text_field = match form.active {
                    0 | 1 => Some(form.active),
                    4 => Some(2),
                    5 => Some(3),
                    _ => None,
                };
                match key.code {
                    KeyCode::Esc => return Ok(false),
                    KeyCode::Tab => form.active = (form.active + 1) % count,
                    KeyCode::BackTab => form.active = (form.active + count - 1) % count,
                    KeyCode::Enter if form.active == 1 => form.fields[1].push('\n'),
                    KeyCode::Enter => form.active = (form.active + 1) % count,
                    KeyCode::Backspace if text_field.is_some() => {
                        form.fields[text_field.unwrap()].pop();
                    }
                    KeyCode::Char(c)
                        if text_field.is_some()
                            && !c.is_control()
                            && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        form.fields[text_field.unwrap()].push(c)
                    }
                    KeyCode::Left | KeyCode::Char('h') if matches!(form.active, 2 | 3) => {
                        let clickup = form
                            .project
                            .checked_sub(1)
                            .and_then(|i| form.projects.get(i))
                            .is_some_and(|p| {
                                self.clickup_projects.iter().any(|c| c.project_id == p.id)
                            });
                        let count = form.projects.len();
                        cycle_field(&mut form, count, false, clickup);
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ')
                        if matches!(form.active, 2 | 3) =>
                    {
                        let clickup = form
                            .project
                            .checked_sub(1)
                            .and_then(|i| form.projects.get(i))
                            .is_some_and(|p| {
                                self.clickup_projects.iter().any(|c| c.project_id == p.id)
                            });
                        let count = form.projects.len();
                        cycle_field(&mut form, count, true, clickup);
                    }
                    _ => {}
                }
                self.mode = Mode::Form(form);
            }
            Mode::Normal => {
                let previous_g = self.pending_g;
                self.pending_g = false;
                match key.code {
                    KeyCode::Char('q') => return Ok(true),
                    KeyCode::Char('b') if self.focus == Focus::Tasks => {
                        if self.moving_bucket {
                            self.message =
                                "A GitHub bucket move is still running; waiting for confirmation."
                                    .into();
                            return Ok(false);
                        }
                        if let Some(task) = self.tasks.get(self.task_index) {
                            if task.github_url.is_some() || task.github_project_url.is_some() {
                                self.bucket_generation += 1;
                                self.bucket_request = Some(crate::github::Request::Buckets(
                                    self.bucket_generation,
                                    task.id,
                                    self.scope(),
                                ));
                                self.mode = Mode::GithubBuckets {
                                    options: vec![],
                                    cursor: 0,
                                    loading: true,
                                };
                            } else {
                                self.message =
                                    "Bucket moves are available for GitHub Project items.".into();
                            }
                        }
                    }
                    KeyCode::Char('?') => self.mode = Mode::Settings { cursor: 0 },
                    KeyCode::Char('I') => self.mode = Mode::SyncIssues { cursor: 0 },
                    KeyCode::Char('m') => {
                        self.assigned_to_me = !self.assigned_to_me;
                        self.task_index = 0;
                        self.tasks.clear();
                        self.refresh(store)?;
                    }
                    KeyCode::Char('f') => {
                        let mut options = vec![
                            SourceFilter::All,
                            SourceFilter::Local,
                            SourceFilter::ClickUp,
                        ];
                        for project in &self.clickup_projects {
                            if project.space.is_empty()
                                || !self.projects.iter().any(|p| p.id == project.project_id)
                            {
                                continue;
                            }
                            let workspace = SourceFilter::Workspace(project.workspace.clone());
                            if !options.contains(&workspace) {
                                options.push(workspace);
                            }
                            let filter = SourceFilter::Space {
                                workspace: project.workspace.clone(),
                                space: project.space.clone(),
                            };
                            if !options.contains(&filter) {
                                options.push(filter);
                            }
                        }
                        options.push(SourceFilter::GitHub);
                        let selected = options.iter().position(|f| f == &self.source).unwrap_or(0);
                        self.mode = Mode::Filter { options, selected };
                    }
                    KeyCode::Char('A') if self.focus == Focus::Tasks => {
                        if let Some(task) = self.tasks.get(self.task_index) {
                            if task.clickup_project || task.github_url.is_some() {
                                let mut members = if task.github_url.is_some() {
                                    store.github_members(task.id)?
                                } else {
                                    store.members(task.project_id.unwrap())?
                                };
                                for assignee in &task.assignees {
                                    if !members.iter().any(|m| m.id == assignee.id) {
                                        members.push(assignee.clone());
                                    }
                                }
                                self.mode = Mode::Assignees {
                                    task: task.id,
                                    members,
                                    selected: task.assignees.iter().map(|a| a.id).collect(),
                                    cursor: 0,
                                    original: task.assignees.clone(),
                                };
                            } else {
                                self.message =
                                    "Assignees are available for ClickUp and GitHub tasks.".into();
                            }
                        }
                    }
                    KeyCode::Char('h') | KeyCode::Left => self.focus = Focus::Projects,
                    KeyCode::Char('l') | KeyCode::Right => self.focus = Focus::Tasks,
                    KeyCode::Tab | KeyCode::BackTab => {
                        self.focus = if self.focus == Focus::Tasks {
                            Focus::Projects
                        } else {
                            Focus::Tasks
                        }
                    }
                    KeyCode::Char('j') | KeyCode::Down => self.move_selection(1, store)?,
                    KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1, store)?,
                    KeyCode::Char('g') if previous_g => self.move_selection(isize::MIN, store)?,
                    KeyCode::Char('g') => self.pending_g = true,
                    KeyCode::Char('G') => self.move_selection(isize::MAX, store)?,
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.move_selection(10, store)?
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.move_selection(-10, store)?
                    }
                    KeyCode::Char('a') => self.form(false, false),
                    KeyCode::Char('P') => self.form(false, true),
                    KeyCode::Char('e') | KeyCode::Enter => {
                        self.form(true, self.focus == Focus::Projects)
                    }
                    KeyCode::Char(' ') if self.focus == Focus::Tasks => {
                        if let Some(task) = self.tasks.get(self.task_index) {
                            store.set_done(task.id, !task.done)?;
                            self.refresh(store)?;
                        }
                    }
                    KeyCode::Char('w') if self.focus == Focus::Tasks => {
                        if let Some(task) = self.tasks.get(self.task_index) {
                            store.set_working(task.id, !task.in_progress)?;
                            self.refresh(store)?;
                        }
                    }
                    KeyCode::Char('p') if self.focus == Focus::Tasks => {
                        if let Some(task) = self.tasks.get(self.task_index) {
                            store.edit_task(
                                task.id,
                                TaskEdit {
                                    title: None,
                                    notes: None,
                                    project: None,
                                    priority: Some(next_priority(
                                        task.priority,
                                        true,
                                        task.clickup_project,
                                    )),
                                    ..TaskEdit::default()
                                },
                            )?;
                            self.refresh(store)?;
                        }
                    }
                    KeyCode::Char('d') => {
                        let project = self.focus == Focus::Projects;
                        let id = if project {
                            self.project_index
                                .checked_sub(2)
                                .and_then(|i| self.projects.get(i))
                                .map(|p| p.id)
                        } else {
                            self.tasks.get(self.task_index).map(|t| t.id)
                        };
                        if let Some(id) = id {
                            self.mode = Mode::Delete { id, project };
                        }
                    }
                    KeyCode::Char('/') => self.mode = Mode::Search(self.search.clone()),
                    KeyCode::Esc => {
                        self.search.clear();
                        self.refresh(store)?;
                    }
                    KeyCode::Char('s') => {
                        self.status = match self.status {
                            Status::Open => Status::All,
                            Status::All => Status::Done,
                            Status::Done => Status::InProgress,
                            Status::InProgress => Status::Open,
                        };
                        self.refresh(store)?;
                    }
                    KeyCode::Char('r') => {
                        self.refresh(store)?;
                        self.message = "Reloaded local data.".into();
                    }
                    _ => {}
                }
            }
        }
        Ok(false)
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 50 || area.height < 16 {
            frame.render_widget(
                Paragraph::new("todo: enlarge terminal to at least 50 x 16.\nPress q to quit.")
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(6),
                Constraint::Length(8),
                Constraint::Length(2),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " todo ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    "  local planner | {:?} | {} tasks",
                    self.status,
                    self.tasks.len()
                )),
                Span::styled(
                    format!("  {}", crate::safe(&self.search)),
                    Style::default().fg(Color::Yellow),
                ),
            ])),
            rows[0],
        );
        frame.render_widget(
            Paragraph::new(format!(
                " Source: {} [f] | {} [m] | Status: {:?} [s] | Search: {} [/]",
                crate::safe(&self.source.label()),
                if self.assigned_to_me {
                    "Assigned to me"
                } else {
                    "All assignees"
                },
                self.status,
                crate::safe(&self.search)
            ))
            .style(Style::default().fg(Color::Cyan)),
            rows[1],
        );
        let panels = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(27), Constraint::Percentage(73)])
            .split(rows[2]);
        let mut project_items = vec![ListItem::new("All tasks"), ListItem::new("Personal")];
        project_items.extend(
            self.projects
                .iter()
                .map(|p| ListItem::new(crate::safe(&self.project_label(Some(p.id))))),
        );
        self.project_state.select(Some(self.project_index));
        frame.render_stateful_widget(
            List::new(project_items)
                .block(panel(" Projects ", self.focus == Focus::Projects))
                .highlight_style(selected(self.focus == Focus::Projects))
                .highlight_symbol("> "),
            panels[0],
            &mut self.project_state,
        );
        self.task_state.select(if self.tasks.is_empty() {
            None
        } else {
            Some(self.task_index)
        });
        let items: Vec<_> = self
            .tasks
            .iter()
            .map(|t| {
                let color = match t.priority {
                    Priority::Urgent => Color::LightRed,
                    Priority::None => Color::DarkGray,
                    Priority::High => Color::Red,
                    Priority::Low => Color::DarkGray,
                    Priority::Normal => Color::White,
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("[{}] ", t.marker()),
                        Style::default().fg(if t.in_progress {
                            Color::Cyan
                        } else {
                            Color::White
                        }),
                    ),
                    Span::styled(format!("{:6} ", t.priority), Style::default().fg(color)),
                    deadline_span(t),
                    Span::styled(
                        format!(
                            "{:<12.12} ",
                            t.assignees
                                .iter()
                                .map(|a| crate::safe(&a.label()))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        if t.github_url.is_some() || t.github_project_url.is_some() {
                            "GH "
                        } else if t.clickup_project {
                            "CU "
                        } else {
                            "   "
                        },
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        if self.current_buckets.is_empty() {
                            String::new()
                        } else {
                            format!(
                                "{:<18.18} ",
                                self.current_buckets
                                    .get(&t.id)
                                    .map(|status| crate::safe(status))
                                    .unwrap_or_default()
                            )
                        },
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::raw(crate::safe(&t.title)),
                ]))
            })
            .collect();
        frame.render_stateful_widget(
            List::new(items)
                .block(panel(" Tasks ", self.focus == Focus::Tasks).title_bottom(
                    if self.current_buckets.is_empty() {
                        " State / Priority / Due / Assignee / Source / Title "
                    } else {
                        " State / Priority / Due / Assignee / Source / Bucket / Title "
                    },
                ))
                .highlight_style(selected(self.focus == Focus::Tasks))
                .highlight_symbol("> "),
            panels[1],
            &mut self.task_state,
        );
        if self.tasks.is_empty() {
            frame.render_widget(
                Paragraph::new("No matching tasks. Press a to add one.").wrap(Wrap { trim: false }),
                Rect {
                    x: panels[1].x + 2,
                    y: panels[1].y + 2,
                    width: panels[1].width.saturating_sub(4),
                    height: panels[1].height.saturating_sub(3),
                },
            );
        }
        let detail = self
            .tasks
            .get(self.task_index)
            .map(|t| {
                format!(
                    "#{} | {} | {} | updated {}\n{} | start: {} | due: {}\nClosed at: {} | Assignees: {}\nSource: {}\n{}{}\n{}",
                    t.id,
                    crate::safe(&self.project_label(t.project_id)),
                    t.priority,
                    t.updated_at,
                    if t.done {
                        "Completed"
                    } else if t.in_progress {
                        "In progress"
                    } else {
                        "Not started"
                    },
                    t.start_date.as_deref().unwrap_or("none"),
                    t.deadline.as_deref().unwrap_or("none"),
                    t.completed_at.as_deref().unwrap_or(if t.done {
                        "unknown (closed before tracking)"
                    } else {
                        "-"
                    }),
                    t.assignees.iter().map(|a| crate::safe(&a.label())).collect::<Vec<_>>().join(", "),
                    t.github_url.clone().or_else(|| t.github_project_url.clone()).unwrap_or_else(|| t.clickup_id.as_ref().map(|id| format!("ClickUp / {id}")).unwrap_or_else(|| if t.clickup_project { "ClickUp (pending upload)".into() } else { "Local".into() })),
                    if self.github_details.is_empty() { String::new() } else { format!("Project: {}\n",crate::safe(&self.github_details)) },
                    crate::safe(&t.title),
                    t.notes
                        .lines()
                        .map(crate::safe)
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            })
            .unwrap_or_else(|| {
                "Personal tasks are unassigned. P creates a project; h/l switch panels.".into()
            });
        frame.render_widget(
            Paragraph::new(detail)
                .block(panel(" Details ", false))
                .wrap(Wrap { trim: false }),
            rows[3],
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(self.actions()),
                Line::styled(
                    format!(
                        "{} | {}",
                        crate::safe(&self.sync_status),
                        crate::safe(&self.message)
                    ),
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            rows[4],
        );
        match &self.mode {
            Mode::GithubBuckets {
                options,
                cursor,
                loading,
            } => {
                let area = popup(frame, 90, 18);
                let items: Vec<_> = if *loading {
                    vec![ListItem::new("Loading Project buckets from GitHub...")]
                } else {
                    options
                        .iter()
                        .map(|b| {
                            ListItem::new(format!(
                                "{} {}",
                                if b.option == b.baseline {
                                    "[current]"
                                } else {
                                    "         "
                                },
                                crate::safe(&b.label)
                            ))
                        })
                        .collect()
                };
                let mut state = ListState::default().with_selected(Some(*cursor));
                frame.render_stateful_widget(
                    List::new(items)
                        .block(panel(
                            " GitHub bucket: j/k select | Enter move | Esc cancel ",
                            true,
                        ))
                        .highlight_style(selected(true))
                        .highlight_symbol("> "),
                    area,
                    &mut state,
                );
                follow_list_cursor(frame, area, &state);
            }
            Mode::Settings { cursor } => {
                let area = popup(frame, 62, 13);
                let mut state = ListState::default().with_selected(Some(*cursor));
                frame.render_stateful_widget(
                    List::new(SETTINGS.iter().enumerate().map(|(index, label)| {
                        if index == 1 && self.github_connected {
                            ListItem::new("GitHub connected")
                                .style(Style::default().fg(Color::DarkGray))
                        } else {
                            ListItem::new(*label)
                        }
                    }))
                    .block(panel(" Settings (j/k, Enter, Esc) ", true))
                    .highlight_style(selected(true))
                    .highlight_symbol("> "),
                    area,
                    &mut state,
                );
                follow_list_cursor(frame, area, &state);
            }
            Mode::GithubSources { cursor } => {
                let area = popup(frame, 86, 20);
                let options = self.source_options();
                let items: Vec<_> = if self.github_sources.is_empty() {
                    vec![ListItem::new(
                        "No sources yet. r: refresh; configure the integration from Settings.",
                    )]
                } else if options.is_empty() {
                    vec![ListItem::new(
                        "No matching sources. / to change the search.",
                    )]
                } else {
                    options
                        .iter()
                        .map(|s| {
                            ListItem::new(format!(
                                "[{}] {}: {}",
                                if s.enabled { "x" } else { " " },
                                s.kind,
                                crate::safe(&s.label)
                            ))
                        })
                        .collect()
                };
                let mut state = ListState::default().with_selected(Some(*cursor));
                frame.render_stateful_widget(
                    List::new(items)
                        .block(panel(
                            &format!(
                                " {}: Space add/remove | r refresh | / search: {}{} ",
                                if self.clickup_source_menu {
                                    "ClickUp"
                                } else {
                                    "GitHub"
                                },
                                crate::safe(&self.source_search),
                                if self.source_search_active { "_" } else { "" }
                            ),
                            true,
                        ))
                        .highlight_style(selected(true))
                        .highlight_symbol("> "),
                    area,
                    &mut state,
                );
                follow_list_cursor(frame, area, &state);
            }
            Mode::SyncIssues { cursor } => {
                let area = popup(frame, 100, 22);
                let items: Vec<_> = if self.sync_issues.is_empty() {
                    vec![ListItem::new(
                        "No sync issues. ClickUp refreshes every 60 seconds.",
                    )]
                } else {
                    self.sync_issues
                        .iter()
                        .map(|message| {
                            let safe = crate::safe(message);
                            let width = area.width.saturating_sub(6).max(1) as usize;
                            let chars: Vec<_> = safe.chars().collect();
                            ListItem::new(
                                chars
                                    .chunks(width)
                                    .map(|part| Line::raw(part.iter().collect::<String>()))
                                    .collect::<Vec<_>>(),
                            )
                        })
                        .collect()
                };
                let mut state = ListState::default().with_selected(Some(*cursor));
                frame.render_stateful_widget(
                    List::new(items)
                        .block(panel(
                            " ClickUp sync issues (j/k scroll, Esc closes) ",
                            true,
                        ))
                        .highlight_style(selected(true)),
                    area,
                    &mut state,
                );
                frame.set_cursor_position((area.x + 1, area.y + 1));
            }
            Mode::Filter {
                options,
                selected: index,
            } => {
                let area = popup(frame, 76, 15);
                let items: Vec<_> = options
                    .iter()
                    .map(|o| ListItem::new(crate::safe(&o.label())))
                    .collect();
                let mut state = ListState::default().with_selected(Some(*index));
                frame.render_stateful_widget(
                    List::new(items)
                        .block(panel(
                            " Source / Space filter (Enter selects, Esc cancels) ",
                            true,
                        ))
                        .highlight_style(selected(true))
                        .highlight_symbol("> "),
                    area,
                    &mut state,
                );
                follow_list_cursor(frame, area, &state);
            }
            Mode::Assignees {
                members,
                selected: ids,
                cursor,
                ..
            } => {
                let area = popup(frame, 76, 17);
                let items: Vec<_> = members
                    .iter()
                    .map(|m| {
                        ListItem::new(format!(
                            "[{}] {}",
                            if ids.contains(&m.id) { "x" } else { " " },
                            crate::safe(&m.label())
                        ))
                    })
                    .collect();
                let mut state =
                    ListState::default().with_selected((!members.is_empty()).then_some(*cursor));
                frame.render_stateful_widget(
                    List::new(items)
                        .block(panel(
                            if members.is_empty() {
                                " Members not loaded yet; wait for sync, then reopen "
                            } else {
                                " Assignees: Space toggles, Enter saves, Esc cancels "
                            },
                            true,
                        ))
                        .highlight_style(selected(true))
                        .highlight_symbol("> "),
                    area,
                    &mut state,
                );
                follow_list_cursor(frame, area, &state);
            }
            Mode::Normal => {
                if self.focus == Focus::Projects {
                    follow_list_cursor(frame, panels[0], &self.project_state);
                } else {
                    follow_list_cursor(frame, panels[1], &self.task_state);
                }
            }
            Mode::Help => {
                let popup = popup(frame, 76, 24);
                frame.render_widget(Paragraph::new(
                    "NAVIGATION\nj/k: move  h/l/Tab: panels  gg/G: first/last\nCtrl-d/u: jump 10   f: source / Workspace / Space\nm: assigned to me   s: status   /: search   Esc: clear\n\nTASKS / PROJECTS\na: add   e/Enter: edit   Space: complete/reopen\nw: start/pause   p: priority   A: assignees\nd: delete (confirmation)   P: new project\nSelect a project before adding; edit Project to move.\n\nCLICKUP\nAutomatic push after edits, pull/discovery every 60s\nI: sync issues   C: import/status setup   S: sync now\n\nFORMS\nType/Backspace: edit   Tab/Shift-Tab: field\nStart/Due: YYYYMMDD or YYYY-MM-DD; blank clears\nh/l or arrows: project/priority   Enter: next/newline\nCtrl-s: save   Esc: cancel   Ctrl-c: quit\n\nr: reload local data   q: quit"
                ).block(panel(" Help ", true)).wrap(Wrap { trim: false }), popup);
            }
            Mode::Search(text) => {
                let popup = popup(frame, 70, 5);
                frame.render_widget(
                    Paragraph::new(format!(
                        "{}_\nEnter: apply | Esc: cancel",
                        crate::safe(text)
                    ))
                    .block(panel(" Search ", true)),
                    popup,
                );
                frame.set_cursor_position((
                    popup.x
                        + 1
                        + Line::raw(crate::safe(text))
                            .width()
                            .min(popup.width.saturating_sub(3) as usize)
                            as u16,
                    popup.y + 1,
                ));
            }
            Mode::Delete { id, project } => {
                let popup = popup(frame, 70, 6);
                let text = if *project {
                    format!("Delete project #{id}?\nIts tasks will become personal, not deleted.\ny: confirm | n/Esc: cancel")
                } else {
                    format!("Permanently delete task #{id}?\ny: confirm | n/Esc: cancel")
                };
                frame.render_widget(
                    Paragraph::new(text)
                        .block(panel(" Confirm deletion ", true))
                        .wrap(Wrap { trim: false }),
                    popup,
                );
            }
            Mode::Form(form) => {
                let popup = popup(frame, 82, if form.project_form { 7 } else { 20 });
                let title = match (form.project_form, form.id.is_some()) {
                    (true, false) => " New project ",
                    (true, true) => " Rename project ",
                    (false, false) => " New task ",
                    (false, true) => " Edit task ",
                };
                let mut lines = Vec::new();
                let mut cursor_row = 0;
                let label = if form.project_form { "Name" } else { "Title" };
                lines.push(field_line(label, &form.fields[0], form.active == 0));
                if !form.project_form {
                    lines.push(field_line("Notes", "", form.active == 1));
                    let notes: Vec<_> = form.fields[1].lines().collect();
                    for line in notes.iter().rev().take(4).rev() {
                        lines.push(Line::raw(format!("  {}", crate::safe(line))));
                    }
                    if form.fields[1].ends_with('\n') {
                        lines.push(Line::raw("  _"));
                    }
                    let project = form
                        .project
                        .checked_sub(1)
                        .and_then(|i| form.projects.get(i))
                        .map(|p| p.id);
                    cursor_row = match form.active {
                        0 => 0,
                        1 => 1,
                        2 => lines.len(),
                        3 => lines.len() + 1,
                        4 => lines.len() + 2,
                        _ => lines.len() + 3,
                    };
                    lines.push(field_line(
                        "Project",
                        &self.project_label(project),
                        form.active == 2,
                    ));
                    lines.push(field_line(
                        "Priority",
                        &form.priority.to_string(),
                        form.active == 3,
                    ));
                    lines.push(field_line(
                        "Deadline (YYYYMMDD or YYYY-MM-DD)",
                        &form.fields[2],
                        form.active == 4,
                    ));
                    lines.push(field_line(
                        "Start date (optional)",
                        &form.fields[3],
                        form.active == 5,
                    ));
                }
                lines.push(Line::raw(""));
                lines.push(Line::raw(
                    "Tab: next | h/l: choices | Ctrl-s: save | Esc: cancel",
                ));
                lines.push(Line::styled(
                    crate::safe(&self.message),
                    Style::default().fg(Color::Yellow),
                ));
                frame.render_widget(
                    Paragraph::new(lines)
                        .block(panel(title, true))
                        .wrap(Wrap { trim: false }),
                    popup,
                );
                frame.set_cursor_position((
                    popup.x + 1,
                    popup.y + 1 + (cursor_row as u16).min(popup.height.saturating_sub(3)),
                ));
            }
        }
    }
}

fn follow_list_cursor(frame: &mut Frame, area: Rect, state: &ListState) {
    if let Some(index) = state.selected() {
        let row = index.saturating_sub(state.offset());
        if area.width > 2 && row < area.height.saturating_sub(2) as usize {
            frame.set_cursor_position((area.x + 1, area.y + 1 + row as u16));
        }
    }
}

fn deadline_span(task: &Task) -> Span<'static> {
    let overdue = task.overdue();
    Span::styled(
        task.deadline
            .as_ref()
            .map(|d| format!("{d}{} ", if overdue { "!" } else { "" }))
            .unwrap_or_else(|| "           ".into()),
        Style::default()
            .fg(if overdue { Color::Red } else { Color::DarkGray })
            .add_modifier(if overdue {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    )
}

fn next_priority(priority: Priority, forward: bool, clickup: bool) -> Priority {
    let values: &[Priority] = if clickup {
        &[
            Priority::None,
            Priority::Low,
            Priority::Normal,
            Priority::High,
            Priority::Urgent,
        ]
    } else {
        &[Priority::Low, Priority::Normal, Priority::High]
    };
    let index = values.iter().position(|p| *p == priority).unwrap_or(1);
    values[if forward {
        (index + 1) % values.len()
    } else {
        (index + values.len() - 1) % values.len()
    }]
}

fn cycle_field(form: &mut Form, project_count: usize, forward: bool, clickup: bool) {
    if form.active == 2 {
        form.project = if forward {
            (form.project + 1) % (project_count + 1)
        } else {
            (form.project + project_count) % (project_count + 1)
        };
    } else {
        form.priority = next_priority(form.priority, forward, clickup);
    }
}

fn field_line(label: &str, value: &str, active: bool) -> Line<'static> {
    Line::styled(
        format!(
            "{} {label}: {}{}",
            if active { ">" } else { " " },
            crate::safe(value),
            if active { " _" } else { "" }
        ),
        Style::default().fg(if active { Color::Cyan } else { Color::White }),
    )
}

fn selected(active: bool) -> Style {
    Style::default()
        .bg(if active { Color::Cyan } else { Color::DarkGray })
        .fg(if active { Color::Black } else { Color::White })
        .add_modifier(Modifier::BOLD)
}

fn panel(title: &str, active: bool) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(if active {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .title(if active {
            format!(" {} [ACTIVE] ", title.trim())
        } else {
            title.to_owned()
        })
        .title_style(if active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        })
        .border_style(Style::default().fg(if active { Color::Cyan } else { Color::DarkGray }))
}

fn popup(frame: &mut Frame, width: u16, height: u16) -> Rect {
    let area = frame.area();
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    let rect = Rect::new(
        (area.width - width) / 2,
        (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    rect
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub fn run(store: &mut Store, database: &std::path::Path) -> Result<()> {
    let mut app = App::new(store)?;
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(io::stdout()))?;
    let mut last_refresh = Instant::now();
    let mut auto_sync = crate::clickup::AutoSync::start(database);
    let mut github_sync = crate::github::Worker::start(database);
    let mut github_actions = crate::github::Worker::start(database);
    let _ = github_sync
        .requests
        .send(crate::github::Request::Sync(true));
    let mut last_sync = Instant::now();
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        if app.pending_command.is_some() || event::poll(Duration::from_millis(250))? {
            let event = if let Some(code) = app.pending_command.take() {
                Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
            } else {
                event::read()?
            };
            if let Event::Key(key) = event {
                if matches!(key.code, KeyCode::Char('C' | 'S' | 'H'))
                    && key.kind != KeyEventKind::Release
                    && matches!(app.mode, Mode::Normal)
                {
                    if key.code == KeyCode::Char('S')
                        && ((app.focus == Focus::Tasks
                            && app.tasks.get(app.task_index).is_some_and(|task| {
                                task.github_url.is_some() || task.github_project_url.is_some()
                            }))
                            || (app.focus == Focus::Projects
                                && match app.scope() {
                                    Scope::Project(id) => store.is_github_project(id)?,
                                    _ => false,
                                }))
                    {
                        let _ = github_sync
                            .requests
                            .send(crate::github::Request::Sync(true));
                        app.message = "GitHub sync queued.".into();
                        continue;
                    }
                    let command = if key.code == KeyCode::Char('H') {
                        None
                    } else if key.code == KeyCode::Char('C') {
                        Some(crate::clickup::Command::Import)
                    } else if let Some(project) = app.sync_project() {
                        Some(crate::clickup::Command::Sync {
                            project,
                            prefer_local: None,
                            prefer_remote: None,
                            prefer_local_all: false,
                            prefer_remote_all: false,
                        })
                    } else {
                        app.message =
                            "Select a ClickUp project or a task in one, then press S.".into();
                        continue;
                    };
                    disable_raw_mode()?;
                    execute!(io::stdout(), LeaveAlternateScreen)?;
                    eprintln!(
                        "Pausing background sync; waiting for any in-flight write to finish..."
                    );
                    auto_sync.stop();
                    github_sync.stop();
                    github_actions.stop();
                    let result = if let Some(command) = command {
                        crate::clickup::run(command, database, false)
                    } else {
                        crate::github::run(crate::github::Command::Connect, database, false)
                    };
                    if let Err(error) = &result {
                        eprintln!("{error:#}");
                    }
                    eprintln!("Press Enter to return to todo.");
                    let mut line = String::new();
                    let _ = io::stdin().read_line(&mut line);
                    enable_raw_mode()?;
                    execute!(io::stdout(), EnterAlternateScreen)?;
                    terminal.clear()?;
                    auto_sync = crate::clickup::AutoSync::start(database);
                    github_sync = crate::github::Worker::start(database);
                    github_actions = crate::github::Worker::start(database);
                    last_sync = Instant::now();
                    app.refresh(store)?;
                    app.message = if result.is_ok() {
                        "Setup finished. Press ? for settings."
                    } else {
                        "Setup needs attention; see the error shown in the setup screen."
                    }
                    .into();
                    continue;
                }
                let before = store.conn.total_changes();
                match app.key(key, store) {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => app.message = format!("{error:#}"),
                }
                if store.conn.total_changes() != before {
                    auto_sync.request(false);
                    let _ = github_sync
                        .requests
                        .send(crate::github::Request::Sync(false));
                    app.sync_status = "ClickUp: syncing edits...".into();
                }
                if std::mem::take(&mut app.refresh_sources) {
                    let _ = github_sync.requests.send(crate::github::Request::Sources);
                    app.message = "Refreshing available GitHub sources (no task import)...".into();
                }
                if let Some(request) = app.bucket_request.take() {
                    if github_actions.requests.send(request).is_err() {
                        app.moving_bucket = false;
                        app.mode = Mode::Normal;
                        app.message = "GitHub action worker stopped; restart todo to retry.".into();
                    }
                }
                if std::mem::take(&mut app.refresh_clickup_sources) {
                    auto_sync.refresh_sources();
                    app.message = "Refreshing available ClickUp Lists (no task import)...".into();
                }
                if std::mem::take(&mut app.sync_clickup_sources) {
                    auto_sync.request(true);
                }
                if std::mem::take(&mut app.sync_sources) {
                    let _ = github_sync
                        .requests
                        .send(crate::github::Request::Sync(true));
                }
            }
        }
        while let Ok((generation, result)) = github_actions.buckets.try_recv() {
            if generation == app.bucket_generation
                && matches!(app.mode, Mode::GithubBuckets { loading: true, .. })
            {
                match result {
                    Ok(options) => {
                        let cursor = options
                            .iter()
                            .position(|b| b.option == b.baseline)
                            .unwrap_or(0);
                        app.mode = Mode::GithubBuckets {
                            options,
                            cursor,
                            loading: false,
                        };
                    }
                    Err(error) => {
                        app.message = format!("GitHub: {error:#}");
                        app.mode = Mode::Normal;
                    }
                }
            }
        }
        while let Ok(result) = github_actions.results.try_recv() {
            app.moving_bucket = false;
            match result {
                Ok(report) => {
                    if let Some(message) = report.message {
                        app.message = message;
                    }
                }
                Err(error) => app.message = format!("GitHub bucket move failed: {error:#}"),
            }
            app.refresh(store)?;
        }
        while let Ok(result) = github_sync.results.try_recv() {
            match result {
                Ok(report) if report.issues.is_empty() => {
                    if let Some(message) = report.message {
                        app.message = message;
                    }
                }
                Ok(report) => app.message = format!("GitHub: {}", report.issues.join("; ")),
                Err(error) => app.message = format!("GitHub: {error:#}"),
            }
            if matches!(app.mode, Mode::Normal | Mode::GithubSources { .. }) {
                app.refresh_if_changed(store)?;
            }
        }
        while let Ok(result) = auto_sync.results.try_recv() {
            app.sync_issues = match &result {
                Ok(report) => report.issues.clone(),
                Err(error) => vec![format!("{error:#}")],
            };
            app.sync_status = match result {
                Ok(report) if report.projects == 0 && report.issues.is_empty() => {
                    "ClickUp: idle / not connected".into()
                }
                Ok(report) if report.issues.is_empty() => {
                    format!("ClickUp: up to date ({} projects)", report.projects)
                }
                Ok(report) => format!("ClickUp: {} issue(s) [I to inspect]", report.issues.len()),
                Err(_) => "ClickUp: offline/pending [I to inspect]".into(),
            };
            if matches!(app.mode, Mode::Normal | Mode::GithubSources { .. }) {
                app.refresh_if_changed(store)?;
            }
        }
        if last_sync.elapsed() >= Duration::from_secs(60) {
            auto_sync.request(true);
            let _ = github_sync
                .requests
                .send(crate::github::Request::Sync(true));
            last_sync = Instant::now();
        }
        if last_refresh.elapsed() >= Duration::from_secs(2) && matches!(app.mode, Mode::Normal) {
            if let Err(error) = app.refresh_if_changed(store) {
                app.message = format!("{error:#}");
            }
            last_refresh = Instant::now();
        }
    }
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn keyboard_creates_project_and_task_and_completes_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("test.db")).unwrap();
        let mut app = App::new(&store).unwrap();
        app.key(key(KeyCode::Char('P')), &mut store).unwrap();
        for c in "Work".chars() {
            app.key(key(KeyCode::Char(c)), &mut store).unwrap();
        }
        app.key(key(KeyCode::Enter), &mut store).unwrap();
        assert!(matches!(app.scope(), Scope::Project(_)));
        app.key(key(KeyCode::Char('a')), &mut store).unwrap();
        for c in "Ship it".chars() {
            app.key(key(KeyCode::Char(c)), &mut store).unwrap();
        }
        app.key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut store,
        )
        .unwrap();
        assert_eq!(app.tasks[0].title, "Ship it");
        assert_eq!(app.tasks[0].project_name.as_deref(), Some("Work"));
        let id = app.tasks[0].id;
        app.key(key(KeyCode::Char(' ')), &mut store).unwrap();
        assert!(app.tasks.is_empty());
        assert!(store.task(id).unwrap().done);
        app.key(key(KeyCode::Char('s')), &mut store).unwrap();
        assert_eq!(app.tasks.len(), 1);
        app.key(key(KeyCode::Char('d')), &mut store).unwrap();
        app.key(key(KeyCode::Char('n')), &mut store).unwrap();
        assert!(store.task(id).is_ok());
        app.key(key(KeyCode::Char('d')), &mut store).unwrap();
        app.key(key(KeyCode::Char('y')), &mut store).unwrap();
        assert!(store.task(id).is_err());
    }

    #[test]
    fn github_bucket_picker_loads_in_background_and_queues_the_selected_move() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("test.db")).unwrap();
        let task = store
            .add_task("Issue", "", None, Priority::Normal, None)
            .unwrap();
        store.conn.execute("INSERT INTO github_issues(url,repository,number,local_id,base_done) VALUES('https://github.com/acme/repo/issues/1','acme/repo',1,?1,0)",[task.id]).unwrap();
        store.conn.execute("INSERT INTO github_sources(kind,remote_id,label,enabled) VALUES('repo','acme/repo','acme/repo',1)",[]).unwrap();
        let mut app = App::new(&store).unwrap();
        app.key(key(KeyCode::Char('b')), &mut store).unwrap();
        assert!(
            matches!(app.bucket_request,Some(crate::github::Request::Buckets(_,id,Scope::All)) if id==task.id)
        );
        assert!(matches!(
            app.mode,
            Mode::GithubBuckets { loading: true, .. }
        ));
        app.key(key(KeyCode::Enter), &mut store).unwrap();
        assert!(matches!(
            app.mode,
            Mode::GithubBuckets { loading: true, .. }
        ));
        let bucket = crate::github::projects::Bucket {
            item: "I1".into(),
            project: "P1".into(),
            field: "F1".into(),
            option: Some("DONE".into()),
            baseline: Some("TODO".into()),
            label: "Board / Done".into(),
            status: Some("Done".into()),
        };
        app.mode = Mode::GithubBuckets {
            options: vec![bucket],
            cursor: 0,
            loading: false,
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        app.key(key(KeyCode::Enter), &mut store).unwrap();
        assert!(
            matches!(app.bucket_request,Some(crate::github::Request::MoveBucket(ref b)) if b.item=="I1")
        );
        assert!(!store.task(task.id).unwrap().done);
        assert!(app.moving_bucket);
        assert!(matches!(app.mode, Mode::Normal));
        app.current_buckets.insert(task.id, "In progress".into());
        terminal.draw(|f| app.draw(f)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(screen.contains("In progress"));
        app.bucket_request = None;
        app.key(key(KeyCode::Char('b')), &mut store).unwrap();
        assert!(
            app.bucket_request.is_none(),
            "A second move must not queue while confirmation is pending"
        );
    }

    #[test]
    fn native_cursor_follows_selection_scrolling_panels_and_popups() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("test.db")).unwrap();
        for i in 0..40 {
            store
                .add_task(&format!("Task {i}"), "", None, Priority::Normal, None)
                .unwrap();
        }
        let mut app = App::new(&store).unwrap();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let first = terminal.get_cursor_position().unwrap();
        app.key(key(KeyCode::Char('j')), &mut store).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let next = terminal.get_cursor_position().unwrap();
        assert_eq!(next.x, first.x);
        assert_eq!(next.y, first.y + 1);
        app.key(key(KeyCode::Char('G')), &mut store).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let scrolled = terminal.get_cursor_position().unwrap();
        assert!(scrolled.y < 20);
        assert!(app.task_state.offset() > 0);
        app.key(key(KeyCode::Char('h')), &mut store).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_eq!(terminal.get_cursor_position().unwrap().x, 1);
        app.key(key(KeyCode::Char('?')), &mut store).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let menu = terminal.get_cursor_position().unwrap();
        app.key(key(KeyCode::Char('j')), &mut store).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_eq!(terminal.get_cursor_position().unwrap().y, menu.y + 1);
    }

    #[test]
    fn renders_empty_and_populated_screens_and_forms() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        let mut app = App::new(&store).unwrap();
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        store
            .add_task("Test", "Notes\nMore notes", None, Priority::High, None)
            .unwrap();
        app.refresh(&store).unwrap();
        app.form(true, false);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app.mode = Mode::Help;
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.resize(Rect::new(0, 0, 20, 5)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
    }

    #[test]
    fn settings_popup_selects_sources_with_search_without_changing_vim_navigation() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("test.db")).unwrap();
        let project = store.add_project("GitHub: acme/repo").unwrap();
        let task = store
            .add_task("Remote issue", "", Some(project.id), Priority::Normal, None)
            .unwrap();
        store.conn.execute("INSERT INTO github_issues(url,repository,number,local_id,base_done) VALUES('https://github.com/acme/repo/issues/1','acme/repo',1,?1,0)", [task.id]).unwrap();
        store.conn.execute("INSERT INTO github_sources(kind,remote_id,label,project_id) VALUES('repo','acme/repo','acme/repo',?1)", [project.id]).unwrap();
        store.conn.execute("INSERT INTO github_sources(kind,remote_id,label) VALUES('repo','other/repo','other/repo')", []).unwrap();
        let mut app = App::new(&store).unwrap();
        assert!(app.tasks.is_empty());
        app.key(key(KeyCode::Char('h')), &mut store).unwrap();
        assert!(app.focus == Focus::Projects);
        app.key(key(KeyCode::Char('l')), &mut store).unwrap();
        app.key(key(KeyCode::Char('?')), &mut store).unwrap();
        assert!(matches!(app.mode, Mode::Settings { .. }));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .contains("Connect GitHub"));
        app.key(key(KeyCode::Enter), &mut store).unwrap();
        assert!(matches!(app.mode, Mode::GithubSources { .. }));
        assert!(
            !app.refresh_sources,
            "Cached source menus do not refresh the whole catalog"
        );
        app.key(key(KeyCode::Char('/')), &mut store).unwrap();
        for c in "ACME".chars() {
            app.key(key(KeyCode::Char(c)), &mut store).unwrap();
        }
        app.key(key(KeyCode::Enter), &mut store).unwrap();
        assert_eq!(app.source_options().len(), 1);
        terminal.draw(|f| app.draw(f)).unwrap();
        app.key(key(KeyCode::Char(' ')), &mut store).unwrap();
        assert_eq!(app.tasks[0].id, task.id);
        assert!(app.sync_sources);
        assert_eq!(app.projects.len(), 1);
        app.key(key(KeyCode::Char(' ')), &mut store).unwrap();
        assert!(app.tasks.is_empty());
        assert!(app.projects.is_empty());
        assert!(store.task(task.id).is_ok());
        app.key(key(KeyCode::Esc), &mut store).unwrap();
        assert!(matches!(app.mode, Mode::Settings { .. }));
        app.key(key(KeyCode::Char('j')), &mut store).unwrap();
        app.key(key(KeyCode::Enter), &mut store).unwrap();
        assert_eq!(app.pending_command, None);
        assert!(matches!(app.mode, Mode::Settings { cursor: 1 }));
        terminal.draw(|f| app.draw(f)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(screen.contains("GitHub connected"));
        assert!(!screen.contains("Connect GitHub"));
        store
            .conn
            .execute("UPDATE github_preferences SET enabled=0", [])
            .unwrap();
        app.refresh(&store).unwrap();
        app.key(key(KeyCode::Enter), &mut store).unwrap();
        assert_eq!(app.pending_command, Some(KeyCode::Char('H')));
        app.pending_command = None;
        app.key(key(KeyCode::Char('?')), &mut store).unwrap();
        for _ in 0..8 {
            app.key(key(KeyCode::Char('j')), &mut store).unwrap();
        }
        app.key(key(KeyCode::Enter), &mut store).unwrap();
        assert!(matches!(app.mode, Mode::Help));
    }

    #[test]
    fn editing_an_issue_keeps_its_hidden_repository_group() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("test.db")).unwrap();
        let repo = store.add_project("GitHub: acme/repo").unwrap();
        let board = store.add_project("Board").unwrap();
        let task = store
            .add_task("Issue", "", Some(repo.id), Priority::Normal, None)
            .unwrap();
        store.conn.execute("INSERT INTO github_issues(url,repository,number,local_id,base_done) VALUES('https://github.com/acme/repo/issues/1','acme/repo',1,?1,0)",[task.id]).unwrap();
        store.conn.execute("INSERT INTO github_projects(project_id,remote_id,owner,title,url) VALUES(?1,'P1','acme','Board','https://github.com/orgs/acme/projects/1')",[board.id]).unwrap();
        store.conn.execute("INSERT INTO github_project_items(remote_id,project_id,local_id,kind,content_id) VALUES('I1',?1,?2,'Issue','issue1')",rusqlite::params![board.id,task.id]).unwrap();
        store.conn.execute("INSERT INTO github_sources(kind,remote_id,label,project_id,enabled) VALUES('repo','acme/repo','acme/repo',?1,0),('project','P1','Board',?2,1)",rusqlite::params![repo.id,board.id]).unwrap();
        let mut app = App::new(&store).unwrap();
        assert_eq!(app.projects.len(), 1);
        app.key(key(KeyCode::Char('e')), &mut store).unwrap();
        app.key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut store,
        )
        .unwrap();
        assert_eq!(store.task(task.id).unwrap().project_id, Some(repo.id));
    }

    #[test]
    fn clickup_sources_use_the_settings_picker_and_github_groups_have_a_badge() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("test.db")).unwrap();
        let project = store.add_project("ClickUp List").unwrap();
        store.conn.execute("INSERT INTO clickup_projects(project_id,list_id,open_status,working_status,done_status,timezone) VALUES(?1,'123','open','working','done','UTC')",[project.id]).unwrap();
        store.conn.execute("INSERT INTO clickup_sources(list_id,label,location) VALUES('123','Work / Space / List','{}')",[]).unwrap();
        let task = store
            .add_task("ClickUp task", "", Some(project.id), Priority::Normal, None)
            .unwrap();
        let github = store.add_project("GitHub: acme/repo").unwrap();
        let board = store.add_project("GitHub Project: acme / Board").unwrap();
        let mut app = App::new(&store).unwrap();
        assert!(app.tasks.is_empty());
        assert_eq!(app.project_label(Some(github.id)), "[GH] acme/repo");
        assert_eq!(app.project_label(Some(board.id)), "[GH] acme / Board");
        app.key(key(KeyCode::Char('?')), &mut store).unwrap();
        for _ in 0..2 {
            app.key(key(KeyCode::Char('j')), &mut store).unwrap();
        }
        app.key(key(KeyCode::Enter), &mut store).unwrap();
        assert!(app.clickup_source_menu);
        assert!(matches!(app.mode, Mode::GithubSources { .. }));
        app.key(key(KeyCode::Char(' ')), &mut store).unwrap();
        assert_eq!(app.tasks[0].id, task.id);
        assert!(app.sync_clickup_sources);
        assert!(crate::clickup::sources(&store).unwrap()[0].enabled);
        store.conn.execute(
            "INSERT INTO clickup_sources(list_id,label,location) VALUES('456','Work / Space / Second List','{}')", []
        ).unwrap();
        app.refresh(&store).unwrap();
        app.key(key(KeyCode::Char('j')), &mut store).unwrap();
        app.key(key(KeyCode::Char(' ')), &mut store).unwrap();
        assert!(matches!(app.mode, Mode::GithubSources { cursor: 1 }));
        assert!(crate::clickup::sources(&store)
            .unwrap()
            .iter()
            .all(|s| s.enabled));
        app.key(key(KeyCode::Char('k')), &mut store).unwrap();
        app.key(key(KeyCode::Char('r')), &mut store).unwrap();
        assert!(app.refresh_clickup_sources);
        assert!(!app.refresh_sources);
        app.key(key(KeyCode::Char(' ')), &mut store).unwrap();
        assert!(app.tasks.is_empty());
        assert!(store.task(task.id).is_ok());
        let sources = crate::clickup::sources(&store).unwrap();
        assert!(!sources[0].enabled);
        assert!(sources[1].enabled);
        app.key(key(KeyCode::Esc), &mut store).unwrap();
        app.mode = Mode::Settings { cursor: 0 };
        app.key(key(KeyCode::Enter), &mut store).unwrap();
        assert!(!app.clickup_source_menu);
        assert!(app.github_sources.is_empty());
    }

    #[test]
    fn periodic_refresh_skips_unchanged_data_and_detects_background_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = Store::open(&path).unwrap();
        let mut app = App::new(&store).unwrap();
        store
            .add_task("Local edit", "", None, Priority::Normal, None)
            .unwrap();
        app.refresh_if_changed(&store).unwrap();
        assert!(app.tasks.is_empty());
        app.refresh(&store).unwrap();
        assert_eq!(app.tasks.len(), 1);
        let worker = Store::open(&path).unwrap();
        worker
            .add_task("Background edit", "", None, Priority::Normal, None)
            .unwrap();
        app.refresh_if_changed(&store).unwrap();
        assert_eq!(app.tasks.len(), 2);
    }

    #[test]
    fn keyboard_edits_deadline_toggles_working_and_rejects_bad_dates() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("test.db")).unwrap();
        let task = store
            .add_task("Test", "", None, Priority::Normal, None)
            .unwrap();
        let mut app = App::new(&store).unwrap();
        app.key(key(KeyCode::Char('e')), &mut store).unwrap();
        for _ in 0..4 {
            app.key(key(KeyCode::Tab), &mut store).unwrap();
        }
        for c in "2027-02-29".chars() {
            app.key(key(KeyCode::Char(c)), &mut store).unwrap();
        }
        let save = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        app.key(save, &mut store).unwrap();
        assert!(matches!(app.mode, Mode::Form(_)));
        assert!(app.message.contains("YYYY-MM-DD"));
        assert!(store.task(task.id).unwrap().deadline.is_none());
        for _ in 0..10 {
            app.key(key(KeyCode::Backspace), &mut store).unwrap();
        }
        for c in "20000101".chars() {
            app.key(key(KeyCode::Char(c)), &mut store).unwrap();
        }
        app.key(save, &mut store).unwrap();
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(
            store.task(task.id).unwrap().deadline.as_deref(),
            Some("2000-01-01")
        );
        assert_eq!(deadline_span(&app.tasks[0]).style.fg, Some(Color::Red));
        app.key(key(KeyCode::Char('w')), &mut store).unwrap();
        assert!(store.task(task.id).unwrap().in_progress);
        app.key(key(KeyCode::Char('w')), &mut store).unwrap();
        assert!(!store.task(task.id).unwrap().in_progress);
        app.key(key(KeyCode::Char('w')), &mut store).unwrap();
        app.key(key(KeyCode::Char(' ')), &mut store).unwrap();
        assert!(store.task(task.id).unwrap().done);
        assert!(!store.task(task.id).unwrap().in_progress);
        assert_ne!(
            deadline_span(&store.task(task.id).unwrap()).style.fg,
            Some(Color::Red)
        );
        app.key(key(KeyCode::Char('s')), &mut store).unwrap();
        app.key(key(KeyCode::Char('w')), &mut store).unwrap();
        assert!(!store.task(task.id).unwrap().done);
        assert!(store.task(task.id).unwrap().in_progress);
        app.form(true, false);
        for _ in 0..4 {
            app.key(key(KeyCode::Tab), &mut store).unwrap();
        }
        for _ in 0..10 {
            app.key(key(KeyCode::Backspace), &mut store).unwrap();
        }
        app.key(save, &mut store).unwrap();
        assert!(store.task(task.id).unwrap().deadline.is_none());
    }

    #[test]
    fn clickup_filter_options_only_include_visible_sidebar_projects() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("test.db")).unwrap();
        for (id, workspace, space, enabled) in [
            ("1", "Visible workspace", "Visible space", true),
            ("2", "Hidden workspace", "Hidden space", false),
            ("3", "Visible workspace", "Other hidden space", false),
        ] {
            let project = store.add_project(&format!("List {id}")).unwrap();
            store.conn.execute(
                "INSERT INTO clickup_projects(project_id,list_id,open_status,working_status,done_status,timezone,workspace,space)
                 VALUES(?1,?2,'open','working','done','UTC',?3,?4)",
                rusqlite::params![project.id,id,workspace,space],
            ).unwrap();
            store.conn.execute(
                "INSERT INTO clickup_sources(list_id,label,location,enabled) VALUES(?1,?1,'{}',?2)",
                rusqlite::params![id,enabled],
            ).unwrap();
        }
        let mut app = App::new(&store).unwrap();
        app.key(key(KeyCode::Char('f')), &mut store).unwrap();
        let Mode::Filter { options, .. } = &app.mode else {
            panic!("Expected filter picker")
        };
        assert!(options.contains(&SourceFilter::Workspace("Visible workspace".into())));
        assert!(options.contains(&SourceFilter::Space {
            workspace: "Visible workspace".into(),
            space: "Visible space".into()
        }));
        assert!(!options
            .iter()
            .any(|o| o.label().contains("Hidden") || o.label().contains("hidden")));
        app.key(key(KeyCode::Esc), &mut store).unwrap();
        crate::clickup::select_source(&store, "2", true).unwrap();
        app.refresh(&store).unwrap();
        app.key(key(KeyCode::Char('f')), &mut store).unwrap();
        let Mode::Filter { options, .. } = &app.mode else {
            panic!("Expected filter picker")
        };
        assert!(options.contains(&SourceFilter::Space {
            workspace: "Hidden workspace".into(),
            space: "Hidden space".into()
        }));
    }

    #[test]
    fn source_filters_and_assignee_picker_keep_local_and_clickup_tasks_separate() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("test.db")).unwrap();
        let press = |app: &mut App, store: &mut Store, code| {
            app.key(key(code), store).unwrap();
        };
        let local = store
            .add_task("Personal", "", None, Priority::High, None)
            .unwrap();
        let project = store.add_project("ClickUp: List (1)").unwrap();
        store.conn.execute("INSERT INTO clickup_projects(project_id,list_id,open_status,working_status,done_status,timezone,workspace,space,members) VALUES (?1,'123','to do','in progress','closed','UTC','Work','Engineering','[{\"id\":7,\"username\":\"Sam\"}]')", [project.id]).unwrap();
        store.conn.execute("UPDATE clickup_projects SET folder='Migration group',list_name='List' WHERE project_id=?1", [project.id]).unwrap();
        let remote = store
            .add_task("Remote", "", Some(project.id), Priority::Urgent, None)
            .unwrap();
        let mut app = App::new(&store).unwrap();
        app.project_index = 1;
        app.refresh(&store).unwrap();
        press(&mut app, &mut store, KeyCode::Char('f'));
        press(&mut app, &mut store, KeyCode::Char('j'));
        press(&mut app, &mut store, KeyCode::Enter);
        assert_eq!(app.project_index, 0);
        assert_eq!(
            app.tasks.iter().map(|t| t.id).collect::<Vec<_>>(),
            [local.id]
        );
        press(&mut app, &mut store, KeyCode::Char('f'));
        press(&mut app, &mut store, KeyCode::Char('j'));
        press(&mut app, &mut store, KeyCode::Enter);
        assert_eq!(
            app.tasks.iter().map(|t| t.id).collect::<Vec<_>>(),
            [remote.id]
        );
        press(&mut app, &mut store, KeyCode::Char('A'));
        press(&mut app, &mut store, KeyCode::Char(' '));
        press(&mut app, &mut store, KeyCode::Enter);
        assert_eq!(store.task(remote.id).unwrap().assignees[0].id, 7);
        press(&mut app, &mut store, KeyCode::Char('f'));
        press(&mut app, &mut store, KeyCode::Char('j'));
        press(&mut app, &mut store, KeyCode::Char('j'));
        press(&mut app, &mut store, KeyCode::Enter);
        assert!(matches!(app.source, SourceFilter::Space { .. }));
        assert_eq!(app.tasks.len(), 1);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Work / Engineering"));
        assert!(screen.contains("Assignees: Sam"));
        assert!(screen.contains("urgent"));
        assert!(screen.contains("[CU] Migration group"));
        assert!(!screen.contains("ClickUp: List (1)"));
        app.project_index = app
            .projects
            .iter()
            .position(|p| p.id == project.id)
            .unwrap()
            + 2;
        app.form(false, false);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Project: [CU] Migration group"));
        assert!(!screen.contains("ClickUp: List (1)"));
    }

    #[test]
    fn github_source_filter_excludes_local_tasks_and_combines_with_assigned_to_me() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("test.db")).unwrap();
        let issue = store
            .add_task("GitHub issue", "", None, Priority::Normal, None)
            .unwrap();
        store
            .add_task("Personal", "", None, Priority::Normal, None)
            .unwrap();
        store
            .conn
            .execute("UPDATE github_preferences SET enabled=1", [])
            .unwrap();
        store.conn.execute("INSERT INTO github_issues(url,repository,number,local_id,base_done) VALUES('https://github.com/acme/repo/issues/1','acme/repo',1,?1,0)", [issue.id]).unwrap();
        store.conn.execute("INSERT INTO github_sources(kind,remote_id,label,enabled) VALUES('repo','acme/repo','acme/repo',1)", []).unwrap();
        let mut app = App::new(&store).unwrap();
        app.key(key(KeyCode::Char('f')), &mut store).unwrap();
        for _ in 0..3 {
            app.key(key(KeyCode::Char('j')), &mut store).unwrap();
        }
        app.key(key(KeyCode::Enter), &mut store).unwrap();
        assert!(matches!(app.source, SourceFilter::GitHub));
        assert_eq!(app.tasks[0].id, issue.id);
        assert_eq!(app.tasks.len(), 1);
        assert!(app.actions().contains("GitHub:"));
        assert!(app.actions().contains("A assignees"));
        store.conn.execute("INSERT INTO github_members(repository,members) VALUES('acme/repo','[{\"id\":7,\"username\":\"sam\"}]')", []).unwrap();
        app.key(key(KeyCode::Char('A')), &mut store).unwrap();
        app.key(key(KeyCode::Char(' ')), &mut store).unwrap();
        app.key(key(KeyCode::Enter), &mut store).unwrap();
        assert_eq!(
            store.task(issue.id).unwrap().assignees[0]
                .username
                .as_deref(),
            Some("sam")
        );
        app.key(key(KeyCode::Char('m')), &mut store).unwrap();
        assert_eq!(app.tasks.len(), 1);
        app.source = SourceFilter::Local;
        app.refresh(&store).unwrap();
        assert!(app.tasks.is_empty());
        app.key(key(KeyCode::Char('m')), &mut store).unwrap();
        assert_eq!(app.tasks[0].title, "Personal");
        assert!(app.actions().contains("Local:"));
        app.focus = Focus::Projects;
        assert!(app.actions().contains("Projects:"));
    }

    #[test]
    fn assigned_to_me_toggle_combines_with_status_and_waits_for_identity() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("test.db")).unwrap();
        let project = store.add_project("ClickUp").unwrap();
        store.conn.execute("INSERT INTO clickup_projects(project_id,list_id,open_status,working_status,done_status,timezone) VALUES (?1,'123','to do','in progress','closed','UTC')", [project.id]).unwrap();
        let mine = store
            .add_task("Shared with me", "", Some(project.id), Priority::High, None)
            .unwrap();
        let other = store
            .add_task("Someone else", "", Some(project.id), Priority::High, None)
            .unwrap();
        store.conn.execute("UPDATE tasks SET assignees='[{\"id\":7,\"username\":\"Sam\"},{\"id\":8,\"username\":\"Other\"}]' WHERE id=?1", [mine.id]).unwrap();
        store
            .conn
            .execute(
                "UPDATE tasks SET assignees='[{\"id\":8,\"username\":\"Other\"}]' WHERE id=?1",
                [other.id],
            )
            .unwrap();
        let mut app = App::new(&store).unwrap();
        app.key(key(KeyCode::Char('m')), &mut store).unwrap();
        assert!(app.tasks.is_empty());
        assert!(app.message.contains("identity"));
        store
            .conn
            .execute("UPDATE clickup_preferences SET user_id=7", [])
            .unwrap();
        app.refresh(&store).unwrap();
        assert_eq!(
            app.tasks.iter().map(|t| t.id).collect::<Vec<_>>(),
            [mine.id]
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .contains("Assigned to me"));
        store.set_done(mine.id, true).unwrap();
        app.refresh(&store).unwrap();
        assert!(app.tasks.is_empty());
        app.status = Status::Done;
        app.refresh(&store).unwrap();
        assert_eq!(app.tasks[0].id, mine.id);
        app.key(key(KeyCode::Char('m')), &mut store).unwrap();
        app.status = Status::All;
        app.refresh(&store).unwrap();
        assert_eq!(app.tasks.len(), 2);
    }

    #[test]
    fn edits_started_before_a_remote_pull_cannot_overwrite_new_remote_content() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("test.db")).unwrap();
        let task = store
            .add_task("Original", "", None, Priority::Normal, None)
            .unwrap();
        let mut app = App::new(&store).unwrap();
        app.key(key(KeyCode::Char('e')), &mut store).unwrap();
        store
            .edit_task(
                task.id,
                TaskEdit {
                    notes: Some("Background update".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE tasks SET updated_at=?1 WHERE id=?2",
                rusqlite::params![task.updated_at, task.id],
            )
            .unwrap();
        app.key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut store,
        )
        .unwrap();
        assert!(matches!(app.mode, Mode::Form(_)));
        assert!(app.message.contains("changed"));
        assert_eq!(store.task(task.id).unwrap().notes, "Background update");
    }

    #[test]
    fn active_panel_indicator_follows_keyboard_focus() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("test.db")).unwrap();
        store
            .add_task("Work", "", None, Priority::Normal, None)
            .unwrap();
        let mut app = App::new(&store).unwrap();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        let screen = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| {
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(screen(&terminal).contains("Tasks [ACTIVE]"));
        assert!(!screen(&terminal).contains("Projects [ACTIVE]"));
        assert_eq!(terminal.backend().buffer()[(30, 3)].bg, Color::Cyan);
        app.key(key(KeyCode::Char('h')), &mut store).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(screen(&terminal).contains("Projects [ACTIVE]"));
        assert!(!screen(&terminal).contains("Tasks [ACTIVE]"));
        assert_eq!(terminal.backend().buffer()[(30, 3)].bg, Color::DarkGray);
        app.key(key(KeyCode::Tab), &mut store).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(screen(&terminal).contains("Tasks [ACTIVE]"));
    }

    #[test]
    fn completed_task_details_show_the_recorded_close_time() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        let task = store
            .add_task("Closed", "", None, Priority::Normal, None)
            .unwrap();
        let completed = store.set_done(task.id, true).unwrap();
        let mut app = App::new(&store).unwrap();
        app.status = Status::Done;
        app.refresh(&store).unwrap();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(150, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(screen.contains("Closed at:"));
        assert!(screen.contains(completed.completed_at.as_deref().unwrap()));
    }

    #[test]
    fn sync_targets_selected_tasks_project_or_sidebar_project() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        let project = store.add_project("ClickUp").unwrap();
        store
            .add_task("Remote", "", Some(project.id), Priority::Normal, None)
            .unwrap();
        let mut app = App::new(&store).unwrap();
        assert_eq!(app.sync_project(), Some(project.id));
        app.focus = Focus::Projects;
        assert_eq!(app.sync_project(), None);
        app.project_index = 2;
        assert_eq!(app.sync_project(), Some(project.id));
        app.project_index = 1;
        app.focus = Focus::Tasks;
        app.refresh(&store).unwrap();
        assert_eq!(app.sync_project(), None);
    }
}
