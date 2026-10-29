mod clickup;
mod github;
mod store;
mod tui;
mod update;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use directories::ProjectDirs;
use serde::Serialize;
use std::{io::IsTerminal, path::PathBuf};
use store::{Priority, Scope, Status, Store, TaskEdit};

#[derive(Parser)]
#[command(
    name = "todo",
    version,
    about = "Local-first tasks and projects with a vim-style terminal UI",
    after_help = "Run without a command to open the TUI.\nAI/automation: use --json with task/project commands. No server is required.\nUse --database PATH for separate workspaces. Shared-list hosting is not implemented."
)]
struct Cli {
    /// SQLite database path (default: your platform's local data directory)
    #[arg(long, global = true, env = "TODO_DATABASE")]
    database: Option<PathBuf>,
    /// Emit machine-readable JSON; errors are JSON on stderr
    #[arg(long, global = true)]
    json: bool,
    /// GitHub owner/repo for releases (or set once with `todo update --repo owner/repo`)
    #[arg(long, global = true, env = "TODO_UPDATE_REPO")]
    update_repo: Option<String>,
    /// Skip the TUI's startup update check
    #[arg(long, global = true, env = "TODO_NO_UPDATE_CHECK")]
    no_update_check: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the interactive terminal UI (also the default)
    Tui,
    /// Read and manage tasks (suitable for scripts and AI agents)
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Read and manage local projects
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Check GitHub Releases and optionally install an update
    Update {
        /// Save the GitHub release repository in local configuration
        #[arg(long)]
        repo: Option<String>,
        /// Only check; never install
        #[arg(long, conflicts_with = "yes")]
        check: bool,
        /// Install without an interactive confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Print storage and configuration paths
    Paths,
    /// Connect, configure automatic ClickUp sync, or sync Lists manually
    Clickup {
        #[command(subcommand)]
        command: clickup::Command,
    },
    /// Authenticate with gh and sync assigned GitHub issues
    Github {
        #[command(subcommand)]
        command: github::Command,
    },
}

#[derive(Args)]
struct Filter {
    /// Only imported tasks assigned to your connected ClickUp/GitHub account
    #[arg(long)]
    assigned_to_me: bool,
    /// Only tasks belonging to this project ID
    #[arg(long, conflicts_with = "personal")]
    project: Option<i64>,
    /// Only unassigned personal tasks
    #[arg(long)]
    personal: bool,
    #[arg(long, value_enum, default_value = "open")]
    status: Status,
    /// Case-insensitive substring in title or notes
    #[arg(long, default_value = "")]
    search: String,
}

impl Filter {
    fn scope(&self) -> Scope {
        match self.project {
            Some(id) => Scope::Project(id),
            None if self.personal => Scope::Personal,
            None => Scope::All,
        }
    }
}

#[derive(Subcommand)]
enum TaskCommand {
    /// List tasks (open tasks by default, ordered by priority then newest)
    List {
        #[command(flatten)]
        filter: Filter,
    },
    /// Read a task including notes and timestamps
    Show { id: i64 },
    /// Create a personal task, or assign it to a project
    Add {
        title: String,
        #[arg(long, default_value = "")]
        notes: String,
        #[arg(long)]
        project: Option<i64>,
        #[arg(long, value_enum, default_value = "normal")]
        priority: Priority,
        /// Due date in YYYYMMDD or YYYY-MM-DD format
        #[arg(long)]
        deadline: Option<String>,
        /// Planned start date (YYYYMMDD or YYYY-MM-DD)
        #[arg(long)]
        start_date: Option<String>,
    },
    /// Update only the supplied fields (use --notes "" to clear notes)
    Edit {
        id: i64,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long, conflicts_with = "personal")]
        project: Option<i64>,
        /// Remove the task's project assignment
        #[arg(long)]
        personal: bool,
        #[arg(long, value_enum)]
        priority: Option<Priority>,
        /// Set the due date (YYYYMMDD or YYYY-MM-DD)
        #[arg(long, conflicts_with = "no_deadline")]
        deadline: Option<String>,
        /// Remove the due date
        #[arg(long)]
        no_deadline: bool,
        /// Planned start date (YYYYMMDD or YYYY-MM-DD)
        #[arg(long, conflicts_with = "no_start_date")]
        start_date: Option<String>,
        #[arg(long)]
        no_start_date: bool,
    },
    /// Replace ClickUp assignees using cached List member IDs (empty clears)
    Assign {
        id: i64,
        #[arg(long, value_delimiter = ',')]
        members: Vec<i64>,
    },
    /// Mark a task in progress (also reopens a completed task)
    Start { id: i64 },
    /// Clear the in-progress marker without completing the task
    Pause { id: i64 },
    /// Mark a task completed (idempotent)
    Done { id: i64 },
    /// Reopen a completed task (idempotent)
    Reopen { id: i64 },
    /// Permanently delete a task
    Delete { id: i64 },
}

#[derive(Subcommand)]
enum ProjectCommand {
    List,
    Add {
        name: String,
    },
    Rename {
        id: i64,
        name: String,
    },
    /// Delete a project; its tasks become personal (not deleted)
    Delete {
        id: i64,
    },
}

#[derive(Serialize)]
struct Paths {
    database: PathBuf,
    config: PathBuf,
}

fn paths(database: Option<PathBuf>) -> Result<Paths> {
    let dirs =
        ProjectDirs::from("", "", "todo").context("Cannot locate your user data directory")?;
    Ok(Paths {
        database: database.unwrap_or_else(|| dirs.data_local_dir().join("todo.sqlite3")),
        config: dirs.config_dir().join("config.json"),
    })
}

fn output<T: Serialize>(json: bool, value: &T, human: impl FnOnce()) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(value)?);
    } else {
        human();
    }
    Ok(())
}

fn safe(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

fn print_task(task: &store::Task) {
    println!(
        "{:>4} [{}] {:6} {:16} {}{}{}",
        task.id,
        task.marker(),
        task.priority,
        safe(task.project_name.as_deref().unwrap_or("Personal")),
        safe(&task.title),
        task.deadline
            .as_ref()
            .map(|d| format!(" | due {d}{}", if task.overdue() { " OVERDUE" } else { "" }))
            .unwrap_or_default(),
        task.completed_at
            .as_ref()
            .map(|time| format!(" | closed {time}"))
            .unwrap_or_default()
    );
    if let Some(start) = &task.start_date {
        println!("     Start: {start}");
    }
    if task.clickup_project {
        println!(
            "     ClickUp {} | Assignees: {}",
            task.clickup_id.as_deref().unwrap_or("(pending)"),
            task.assignees
                .iter()
                .map(|a| safe(&a.label()))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

fn run(cli: Cli) -> Result<()> {
    let paths = paths(cli.database)?;
    let json = cli.json;
    match cli.command.unwrap_or(Command::Tui) {
        Command::Clickup { command } => clickup::run(command, &paths.database, json),
        Command::Github { command } => github::run(command, &paths.database, json),
        Command::Paths => output(json, &paths, || {
            println!(
                "Database: {}\nConfig:   {}",
                paths.database.display(),
                paths.config.display()
            );
        }),
        Command::Update { repo, check, yes } => {
            if let Some(repo) = repo.as_deref() {
                update::save_repo(&paths.config, repo)?;
            }
            let repo = repo
                .or(cli.update_repo)
                .or(update::configured_repo(&paths.config)?);
            let repo = repo.context(
                "No release repository configured. Run: todo update --repo OWNER/REPO --check",
            )?;
            update::run(&repo, check || json, yes, json)
        }
        Command::Tui => {
            if json {
                bail!("The TUI does not support --json; use task or project commands");
            }
            if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                bail!("The TUI requires a terminal. For automation use: todo --json task list");
            }
            let mut store = Store::open(&paths.database)?;
            if !cli.no_update_check {
                // Updates never block task access when offline or misconfigured.
                let result = (|| -> Result<()> {
                    if let Some(repo) = cli.update_repo.or(update::configured_repo(&paths.config)?)
                    {
                        update::run(&repo, false, false, false)?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    eprintln!("Update check skipped: {error:#}");
                }
            }
            tui::run(&mut store, &paths.database)
        }
        Command::Project { command } => {
            let mut store = Store::open(&paths.database)?;
            match command {
                ProjectCommand::List => {
                    let projects = store.visible_projects()?;
                    output(json, &projects, || {
                        for project in &projects {
                            println!("{:>4}  {}", project.id, safe(&project.name));
                        }
                    })
                }
                ProjectCommand::Add { name } => {
                    let project = store.add_project(&name)?;
                    output(json, &project, || {
                        println!("Created project {}: {}", project.id, project.name)
                    })
                }
                ProjectCommand::Rename { id, name } => {
                    store.rename_project(id, &name)?;
                    output(
                        json,
                        &serde_json::json!({"id": id, "renamed": true}),
                        || println!("Renamed project {id}"),
                    )
                }
                ProjectCommand::Delete { id } => {
                    store.delete_project(id)?;
                    output(
                        json,
                        &serde_json::json!({"id": id, "deleted": true}),
                        || println!("Deleted project {id}; its tasks are now personal"),
                    )
                }
            }
        }
        Command::Task { command } => {
            let store = Store::open(&paths.database)?;
            let mutates = !matches!(
                command,
                TaskCommand::List { .. } | TaskCommand::Show { .. } | TaskCommand::Delete { .. }
            );
            let task = match command {
                TaskCommand::List { filter } => {
                    let mut tasks = store.tasks(filter.scope(), filter.status, &filter.search)?;
                    if filter.assigned_to_me {
                        store.assigned_to_me(&mut tasks)?;
                    }
                    return output(json, &tasks, || {
                        for task in &tasks {
                            print_task(task);
                        }
                    });
                }
                TaskCommand::Delete { id } => {
                    store.delete_task(id)?;
                    return output(
                        json,
                        &serde_json::json!({"id": id, "deleted": true}),
                        || println!("Deleted task {id}"),
                    );
                }
                TaskCommand::Show { id } => store.task(id)?,
                TaskCommand::Add {
                    title,
                    notes,
                    project,
                    priority,
                    deadline,
                    start_date,
                } => {
                    // Validate all fields before creating the task.
                    store.conn.execute_batch("BEGIN IMMEDIATE")?;
                    let result = (|| -> Result<_> {
                        let task = store.add_task(
                            &title,
                            &notes,
                            project,
                            priority,
                            deadline.as_deref(),
                        )?;
                        store.edit_task(
                            task.id,
                            TaskEdit {
                                start_date: start_date.map(Some),
                                ..Default::default()
                            },
                        )
                    })();
                    match result {
                        Ok(task) => {
                            store.conn.execute_batch("COMMIT")?;
                            task
                        }
                        Err(error) => {
                            store.conn.execute_batch("ROLLBACK")?;
                            return Err(error);
                        }
                    }
                }
                TaskCommand::Edit {
                    id,
                    title,
                    notes,
                    project,
                    personal,
                    priority,
                    deadline,
                    no_deadline,
                    start_date,
                    no_start_date,
                } => {
                    if title.is_none()
                        && notes.is_none()
                        && project.is_none()
                        && !personal
                        && priority.is_none()
                        && deadline.is_none()
                        && !no_deadline
                        && start_date.is_none()
                        && !no_start_date
                    {
                        bail!("Supply at least one field to edit (see todo task edit --help)");
                    }
                    store.edit_task(
                        id,
                        TaskEdit {
                            title,
                            notes,
                            priority,
                            deadline: if no_deadline {
                                Some(None)
                            } else {
                                deadline.map(Some)
                            },
                            project: if personal {
                                Some(None)
                            } else {
                                project.map(Some)
                            },
                            start_date: if no_start_date {
                                Some(None)
                            } else {
                                start_date.map(Some)
                            },
                            ..TaskEdit::default()
                        },
                    )?
                }
                TaskCommand::Done { id } => store.set_done(id, true)?,
                TaskCommand::Reopen { id } => store.set_done(id, false)?,
                TaskCommand::Start { id } => store.set_working(id, true)?,
                TaskCommand::Pause { id } => store.set_working(id, false)?,
                TaskCommand::Assign { id, members } => store.assign(id, &members)?,
            };
            if mutates && task.clickup_project {
                match clickup::push_after_edit(&paths.database) {
                    Ok(report) => {
                        for issue in report.issues {
                            eprintln!("ClickUp: {}", safe(&issue));
                        }
                    }
                    Err(error) => eprintln!(
                        "Saved locally; ClickUp will retry while the TUI is open: {error:#}"
                    ),
                }
            }
            if mutates && task.github_url.is_some() && github::enabled(&store)? {
                match github::sync(&paths.database, false) {
                    Ok(report) => {
                        for issue in report.issues {
                            eprintln!("GitHub: {}", safe(&issue));
                        }
                    }
                    Err(error) => eprintln!("Saved locally; GitHub sync pending: {error:#}"),
                }
            }
            let task = store.task(task.id)?;
            output(json, &task, || {
                print_task(&task);
                if !task.notes.is_empty() {
                    // Keep newlines but never send terminal escape sequences from stored notes.
                    for line in task.notes.lines() {
                        println!("     {}", safe(line));
                    }
                }
            })
        }
    }
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            if json {
                eprintln!("{}", serde_json::json!({"error": format!("{error:#}")}));
            } else {
                eprintln!("Error: {error:#}");
            }
            std::process::ExitCode::FAILURE
        }
    }
}
