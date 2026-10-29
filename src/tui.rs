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
}

#[derive(Clone, PartialEq)]
enum SourceFilter {
    All,
    Local,
    ClickUp,
    Workspace(String),
    Space { workspace: String, space: String },
}

impl SourceFilter {
    fn label(&self) -> String {
        match self {
            Self::All => "All sources".into(),
            Self::Local => "Local".into(),
            Self::ClickUp => "ClickUp".into(),
            Self::Workspace(workspace) => format!("{workspace} / All Spaces"),
            Self::Space { workspace, space } => format!("{workspace} / {space}"),
        }
    }
}

enum Mode {
    Normal,
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
            message: "Local-first. Press ? for help.".into(),
            mode: Mode::Normal,
            pending_g: false,
            clickup_projects: Vec::new(),
            source: SourceFilter::All,
            sync_status: "ClickUp: connecting...".into(),
            sync_issues: Vec::new(),
        };
        app.refresh(store)?;
        Ok(app)
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
        let project_id = match self.scope() {
            Scope::Project(id) => Some(id),
            _ => None,
        };
        let task_id = self.tasks.get(self.task_index).map(|t| t.id);
        self.projects = store.projects()?;
        self.clickup_projects = store.clickup_projects()?;
        if let Some(id) = project_id {
            self.project_index = self
                .projects
                .iter()
                .position(|p| p.id == id)
                .map_or(0, |i| i + 2);
        }
        self.tasks = store.tasks(self.scope(), self.status, &self.search)?;
        self.tasks.retain(|task| match &self.source {
            SourceFilter::All => true,
            SourceFilter::Local => !task.clickup_project && task.clickup_id.is_none(),
            SourceFilter::ClickUp => task.clickup_project || task.clickup_id.is_some(),
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
        self.task_index = task_id
            .and_then(|id| self.tasks.iter().position(|t| t.id == id))
            .unwrap_or(self.task_index)
            .min(self.tasks.len().saturating_sub(1));
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
                    .and_then(|id| self.projects.iter().position(|p| p.id == id))
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
                .and_then(|i| self.projects.get(i))
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
        self.message = "Saved locally. Linked ClickUp changes sync automatically.".into();
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
                            self.message = "Assignees saved; ClickUp sync queued.".into();
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
                            .and_then(|i| self.projects.get(i))
                            .is_some_and(|p| {
                                self.clickup_projects.iter().any(|c| c.project_id == p.id)
                            });
                        cycle_field(&mut form, self.projects.len(), false, clickup);
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ')
                        if matches!(form.active, 2 | 3) =>
                    {
                        let clickup = form
                            .project
                            .checked_sub(1)
                            .and_then(|i| self.projects.get(i))
                            .is_some_and(|p| {
                                self.clickup_projects.iter().any(|c| c.project_id == p.id)
                            });
                        cycle_field(&mut form, self.projects.len(), true, clickup);
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
                    KeyCode::Char('?') => self.mode = Mode::Help,
                    KeyCode::Char('I') => self.mode = Mode::SyncIssues { cursor: 0 },
                    KeyCode::Char('f') => {
                        let mut options = vec![
                            SourceFilter::All,
                            SourceFilter::Local,
                            SourceFilter::ClickUp,
                        ];
                        for project in &self.clickup_projects {
                            if project.space.is_empty() {
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
                        let selected = options.iter().position(|f| f == &self.source).unwrap_or(0);
                        self.mode = Mode::Filter { options, selected };
                    }
                    KeyCode::Char('A') if self.focus == Focus::Tasks => {
                        if let Some(task) = self.tasks.get(self.task_index) {
                            if task.clickup_project {
                                let mut members = store.members(task.project_id.unwrap())?;
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
                                    "Assignees are available for ClickUp tasks only.".into();
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
                " Source: {} [f]  | Status: {:?} [s] | Search: {} [/]",
                crate::safe(&self.source.label()),
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
        project_items.extend(self.projects.iter().map(|p| {
            if let Some(link) = self.clickup_projects.iter().find(|c| c.project_id == p.id) {
                let group = link
                    .folder
                    .as_deref()
                    .filter(|name| !name.is_empty())
                    .unwrap_or(&link.space);
                let label = if group.is_empty() {
                    p.name.clone()
                } else if link.list_name.is_empty() || link.list_name.eq_ignore_ascii_case("list") {
                    group.to_owned()
                } else {
                    format!("{group} / {}", link.list_name)
                };
                ListItem::new(format!("[CU] {}", crate::safe(&label)))
            } else {
                ListItem::new(crate::safe(&p.name))
            }
        }));
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
                        if t.clickup_project { "CU " } else { "   " },
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(crate::safe(&t.title)),
                ]))
            })
            .collect();
        frame.render_stateful_widget(
            List::new(items)
                .block(
                    panel(" Tasks ", self.focus == Focus::Tasks)
                        .title_bottom(" State / Priority / Due / Assignee / Source / Title "),
                )
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
                    "#{} | {} | {} | updated {}\n{} | start: {} | due: {}\nClosed at: {} | Assignees: {}\nSource: {}\n{}\n{}",
                    t.id,
                    crate::safe(t.project_name.as_deref().unwrap_or("Personal")),
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
                    t.clickup_id.as_ref().map(|id| format!("ClickUp / {id}")).unwrap_or_else(|| if t.clickup_project { "ClickUp (pending upload)".into() } else { "Local".into() }),
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
        frame.render_widget(Paragraph::new(vec![
            Line::from(" j/k move  h/l panels  a add  e edit  A assign  f filter  w work  Space done  S sync  C import  ? help"),
            Line::styled(format!("{} | {}", crate::safe(&self.sync_status), crate::safe(&self.message)), Style::default().fg(Color::Yellow)),
        ]), rows[4]);
        match &self.mode {
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
            }
            Mode::Normal => {}
            Mode::Help => {
                let popup = popup(frame, 76, 24);
                frame.render_widget(Paragraph::new(
                    "NAVIGATION\nj/k: move  h/l/Tab: panels  gg/G: first/last\nCtrl-d/u: jump 10   f: source / Workspace / Space\ns: open/all/done/in-progress   /: search   Esc: clear\n\nTASKS / PROJECTS\na: add   e/Enter: edit   Space: complete/reopen\nw: start/pause   p: priority   A: assignees\nd: delete (confirmation)   P: new project\nSelect a project before adding; edit Project to move.\n\nCLICKUP\nAutomatic push after edits, pull/discovery every 60s\nI: sync issues   C: import/status setup   S: sync now\n\nFORMS\nType/Backspace: edit   Tab/Shift-Tab: field\nStart/Due: YYYYMMDD or YYYY-MM-DD; blank clears\nh/l or arrows: project/priority   Enter: next/newline\nCtrl-s: save   Esc: cancel   Ctrl-c: quit\n\nr: reload local data   q: quit"
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
                        .and_then(|i| self.projects.get(i))
                        .map_or("Personal", |p| p.name.as_str());
                    lines.push(field_line("Project", project, form.active == 2));
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
            }
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
    let mut last_sync = Instant::now();
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if matches!(key.code, KeyCode::Char('C' | 'S'))
                    && key.kind != KeyEventKind::Release
                    && matches!(app.mode, Mode::Normal)
                {
                    let command = if key.code == KeyCode::Char('C') {
                        crate::clickup::Command::Import
                    } else if let Some(project) = app.sync_project() {
                        crate::clickup::Command::Sync {
                            project,
                            prefer_local: None,
                            prefer_remote: None,
                        }
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
                    let result = crate::clickup::run(command, database, false);
                    if let Err(error) = &result {
                        eprintln!("ClickUp: {error:#}");
                    }
                    eprintln!("Press Enter to return to todo.");
                    let mut line = String::new();
                    let _ = io::stdin().read_line(&mut line);
                    enable_raw_mode()?;
                    execute!(io::stdout(), EnterAlternateScreen)?;
                    terminal.clear()?;
                    auto_sync = crate::clickup::AutoSync::start(database);
                    last_sync = Instant::now();
                    app.refresh(store)?;
                    app.message = if result.is_ok() {
                        "ClickUp operation finished."
                    } else {
                        "ClickUp needs attention; see the error shown in the sync/import screen."
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
                    app.sync_status = "ClickUp: syncing edits...".into();
                }
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
            if matches!(app.mode, Mode::Normal) {
                app.refresh(store)?;
            }
        }
        if last_sync.elapsed() >= Duration::from_secs(60) {
            auto_sync.request(true);
            last_sync = Instant::now();
        }
        if last_refresh.elapsed() >= Duration::from_secs(2) && matches!(app.mode, Mode::Normal) {
            if let Err(error) = app.refresh(store) {
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
    fn source_filters_and_assignee_picker_keep_local_and_clickup_tasks_separate() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("test.db")).unwrap();
        let press = |app: &mut App, store: &mut Store, code| {
            app.key(key(code), store).unwrap();
        };
        let local = store
            .add_task("Personal", "", None, Priority::High, None)
            .unwrap();
        let project = store.add_project("Remote").unwrap();
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
