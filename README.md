# todo

A local-first TODO list and project planner written in Rust. Run `todo` for a
lazygit-style terminal interface with vim navigation, or use its CLI as a
machine-readable API. Works on Linux and macOS, including Apple Silicon.

## Available now

- Personal, unassigned tasks and named projects.
- Add, edit, complete, reopen, search, prioritize, move, and delete tasks.
- Date-only deadlines with overdue highlighting and an in-progress task marker.
- Task notes, creation/update timestamps, and open/completed/all/in-progress filters.
- Local SQLite persistence, separate workspaces, and JSON commands for AI agents.
- GitHub Release checks at TUI startup, with confirmation before self-updating.
- Release automation and a Homebrew formula template for distribution.
- Automatic two-way ClickUp sync, List discovery, assignees, and planned start/due dates.
- Source, Workspace, and Space filters with exact ClickUp priorities.

Local planning needs no account, background service, or internet connection.
The optional ClickUp integration requires your ClickUp account and network access.

## Install from source

Install the current stable Rust toolchain, then from this directory:

```sh
cargo install --path . --locked
todo
```

Ensure `~/.cargo/bin` is on your `PATH`. For development, use `cargo run --`.
The terminal UI needs an interactive terminal, preferably at least 80 columns
and 24 rows. SQLite is bundled; no separate database installation is needed.

## Homebrew

**Not published to Homebrew yet.** The release workflow generates a ready-to-use
`todo.rb` formula with the actual release URLs and SHA-256 checksums for Intel
and ARM macOS/Linux.

To publish through your own tap:

1. Publish a GitHub Release using the instructions below.
2. Create a public repository named `homebrew-tap` under your GitHub account or organization.
3. Copy the release's `todo.rb` asset into `Formula/todo.rb` in that tap and commit it.

Users can then install it with the following commands, replacing `OWNER`:

```sh
brew tap OWNER/tap
brew install OWNER/tap/todo
todo
```

For every subsequent release, copy the new formula into the tap. Users update
with `brew update && brew upgrade todo`. When `todo` detects a Homebrew
installation, it suggests that command instead of replacing the managed binary.
A tap is separate from Homebrew/core; a core submission would be a later step,
subject to Homebrew's acceptance requirements.

## Terminal interface

The sidebar contains **All tasks**, **Personal**, and your projects. Select a
project before adding a task to assign it automatically. Adding from All tasks
or Personal creates an unassigned task. Empty lists are normal on first launch.
The focused panel has a thick cyan border, an `[ACTIVE]` title, and a cyan
selection; the other panel stays muted. Use `h`/`l` or `Tab` to switch focus.

| Key | Action |
| --- | --- |
| `j` / `k`, arrows | Move selection |
| `h` / `l`, `Tab` | Switch sidebar/task panel |
| `gg` / `G` | First/last item |
| `Ctrl-d` / `Ctrl-u` | Jump down/up ten items |
| `a` | Add a task |
| `e` / `Enter` | Edit selected task, or rename selected project |
| `Space` | Complete/reopen selected task |
| `w` | Toggle "in progress" (`[>]`); starting a completed task reopens it |
| `p` | Cycle task priority |
| `P` | Create a project |
| `C` | Discover ClickUp Lists, select them, and import into separate projects |
| `S` | Sync the selected task's project or the selected project with ClickUp |
| `f` | Filter by All sources, Local, ClickUp, Workspace, or Space |
| `A` | Change ClickUp assignees; `Space` toggles members, `Enter` saves |
| `I` | Inspect automatic-sync errors and conflicts |
| `d` | Delete selected task/project, after confirmation |
| `/` | Search titles and notes in the current list |
| `Esc` | Clear search |
| `s` | Cycle open/all/completed/in-progress tasks |
| `r` | Reload local data |
| `?` | Help |
| `q` / `Ctrl-c` | Quit |

Forms start in text-entry mode: type to append, `Backspace` to remove, and
`Tab`/`Shift-Tab` to change fields. Use `h`/`l` or left/right arrows on the
Project and Priority fields. `Enter` inserts a newline in notes and advances
other fields. `Ctrl-s` saves; `Esc` cancels. A project's name also saves with
`Enter`. Change a task's Project field to move it, or select Personal to unassign it.

Set the optional Deadline and Start date fields to `YYYYMMDD` (for example `20260601`) or
`YYYY-MM-DD`; leave them blank to clear them. Both the TUI and CLI accept
either format; stored dates and JSON output use `YYYY-MM-DD`.
Unfinished tasks with a deadline before today (your local date)
show a bold red date with `!`. Tasks due today are not overdue; completed tasks
are never highlighted as overdue. Press `w` to mark work in progress (`[>]`)
or pause it. Completing or reopening a task clears its in-progress marker.
The default open view includes both unstarted and in-progress tasks.
Task details show **Closed at** after completion, and CLI/JSON output includes
`completed_at` as a UTC timestamp. Completing an already completed task preserves
that time. Reopening/starting it clears the timestamp; closing it again records
a new time. Tasks closed before this feature have an unknown time (`null` in JSON),
not an invented historical timestamp. Imported ClickUp tasks use ClickUp's close
time when available.

The task panel shows status, priority, due date, assignees, source (`CU` for
ClickUp), and title in aligned columns. Details include planned start/due dates
and actual completion time separately. Press `f` to narrow the source; selecting
a source resets the sidebar to All tasks, then you can choose a specific List's
project. Source, status, project, and search filters combine.

Deleting a project preserves its tasks by moving them to Personal. Deleting a
task is permanent. The TUI refreshes every two seconds while outside a form,
so changes made by a local CLI/agent appear without restarting.

## CLI and AI agents

`todo --help`, `todo task --help`, and each command's `--help` describe the API.
No custom skill, HTTP server, SDK, or direct database access is required.
`-h` is short help; `--help` includes longer descriptions.

```sh
todo project add "Website"
todo project list
todo task add "Book dentist" --priority high
todo task add "Send report" --deadline 20261201
todo task add "Plan release" --start-date 20261101 --deadline 20261201
todo task add "Design landing page" --project 1 --notes "Prepare two options"
todo task list --project 1
todo task list --personal
todo task list --status all --search landing
todo task show 2
todo task edit 2 --title "Design homepage" --priority low
todo task edit 2 --personal
todo task edit 2 --notes ""
todo task edit 2 --deadline 2026-12-15
todo task edit 2 --no-deadline
todo task edit 2 --start-date 20261115
todo task edit 2 --no-start-date
todo task start 2
todo task list --status in-progress
todo task pause 2
todo task done 2
todo task reopen 2
todo project rename 1 "New website"
todo task delete 2
todo project delete 1
```

IDs in these examples assume a fresh database. Use IDs returned by add/list
commands in scripts; do not infer them from names or ordering.

Add `--json` for machine-readable output:

```sh
todo --json project list
todo --json task add "Review changes" --project 1
todo --json task list --status all
todo --json task edit 3 --notes "Reviewed by agent"
```

List commands return arrays; task mutations return the resulting task.
Task JSON includes `start_date` and `deadline` (date strings or `null`),
`assignees` (member IDs and names), `clickup_id`, `clickup_project`, and
`in_progress`, alongside the existing `done` flag.
Deletes return `{"id":3,"deleted":true}`. Runtime failures return
`{"error":"..."}` on stderr with a nonzero exit code. Argument/usage errors
are reported by the CLI parser as text. Successful JSON commands write only
JSON to stdout and never run startup update checks. `--json update` only checks;
it never installs. Unknown IDs and invalid project assignments are errors.

An agent instruction can simply say: "Run `todo --help` and
`todo task --help`; read and change tasks using `todo --json ...`.
Only mark work complete after verifying it." Agents and the TUI can use the
same local database concurrently.

## ClickUp integration

Each ClickUp List maps to a local project. Once connected, `todo` automatically
discovers **all accessible Lists** at TUI startup, including new Lists, and
refreshes them every **60 seconds** while the TUI is open. Local edits trigger
a debounced background sync; CLI task mutations also attempt to sync linked
tasks. No separate daemon is needed, and the UI remains usable during sync.
Titles, Markdown descriptions, exact priorities, assignees, planned start/due
dates, and completion/in-progress state sync in both directions. Unrelated
personal projects and tasks are never uploaded.

Tasks save locally first. Offline edits remain stored and retry on the next
cycle or launch. Sync issues appear in the footer; press **`I`** for details.
Conflicting edits are never silently overwritten. A form opened before a
background update must be reopened before saving.

Automatic discovery uses your ClickUp profile timezone, or an existing link's
timezone. If neither is available, set it once:

```sh
todo clickup auto --timezone Europe/Stockholm
todo clickup auto --disable  # Keep local data; use C/S or CLI for manual sync.
todo clickup auto --enable
```

These settings are per database. Automatic sync stops when the TUI closes;
remaining local edits are picked up on its next launch. Disabling automatic
sync also disables the automatic push after CLI mutations.

### Connect your account

```sh
todo clickup connect
```

This opens [ClickUp Settings > Apps](https://app.clickup.com/settings/apps) in
your browser. Sign in, generate/copy your **personal API token**, then paste it
into the hidden terminal prompt. The token is validated before being saved in
macOS Keychain or Linux Secret Service. It is never saved in the SQLite database,
configuration file, or command-line arguments. Browser-assisted setup does not
retrieve a token automatically and is not OAuth.

Use `todo clickup connect --no-browser` if you want to open settings yourself.
Linux requires a running, unlocked Secret Service provider (e.g. GNOME Keyring
or a compatible password manager) and a session D-Bus. On headless systems or
if secure storage is unavailable, supply `CLICKUP_API_TOKEN` through your shell
or secret manager instead. An environment token takes precedence over a saved
token; there is no plaintext storage fallback.

`todo clickup disconnect` removes the saved token but does not revoke it remotely
or unset environment variables. Revoke a token from ClickUp's Apps settings.
This is a personal-token integration; an OAuth app/sign-in flow for multi-user
distribution is not implemented.

### Link a List and sync

**Normally:** connect and start `todo`; accessible Lists are linked/imported
automatically. Standard, unambiguous status mappings are detected. If a List's
statuses are ambiguous, it is skipped with an issue: press **`C`** to select its
open/working/completed statuses once. You can also import manually:

```sh
todo clickup import
```

The picker discovers your workspaces, Spaces, folderless Lists and Lists inside
Folders. Choose List numbers (e.g. `1,3`), confirm your ClickUp timezone, and
select the matching task statuses. Each selected List is imported into its own
new `ClickUp: <List name>` project; existing personal projects are untouched.
Selecting an already linked List syncs its existing project instead of creating
a duplicate. Cancel with an empty selection. Automatic discovery includes all
accessible Lists regardless of the manual picker's selection; disable it above
if you want only manually selected Lists.

Discovery only shows non-archived Lists your token can access through the
workspace hierarchy. Permission failures are reported without hiding Lists
found elsewhere; guest-shared Lists outside accessible Spaces may still need
manual linking. To list discovered IDs without importing:

```sh
todo clickup discover
todo --json clickup discover
```

For manual linking or automation:

Find your ClickUp List ID by copying its link in ClickUp (the number following
`/li/` in that URL). Inspect its statuses, then create/link a local project:

```sh
todo clickup list 123456789
todo project add "ClickUp work"
todo project list
todo clickup link 1 123456789 \
  --timezone Europe/Stockholm \
  --open-status "to do" \
  --working-status "in progress" \
  --done-status "closed"
todo clickup sync 1
```

Replace the IDs, timezone, and status names with your own. Names must exactly
match statuses returned by `clickup list`; open/working statuses must be unfinished
and the done status must be a ClickUp `done` or `closed` type. The timezone must
match your ClickUp profile so date-only deadlines stay on the correct day.
Each local project and remote List can be linked only once in a database.
**Every existing task in the linked local project will be uploaded on sync.**
Start with an empty project if you only want to import first.

Use imported tasks normally (`w` to work, `Space` to complete); edits,
including descriptions, are sent automatically. Press **`S`** to sync immediately
or run `todo clickup sync 1`. Remote changes appear on the next automatic pull.

ClickUp tasks use **Urgent / High / Normal / Low / None**; personal tasks retain
**High / Normal / Low**. Press **`A`** to view/change a task's assignees using
cached explicit List members and existing assignees, refreshed during sync.
ClickUp's List-members API omits users who only inherit access through a Team,
Folder, or Space; those users appear here if already assigned to the task.
CLI equivalents:

```sh
todo clickup members 1
todo task assign 42 --members 123,456
todo task assign 42 --members ""  # Remove all assignees.
todo task edit 42 --priority urgent --start-date 20260601 --deadline 20260615
todo task edit 42 --priority none --no-start-date --no-deadline
```

Planned finish is the due date, not the actual close timestamp. Local start/due
edits write date-only values in the linked List's configured timezone.

```sh
todo clickup links
todo --json clickup sync 1
```

Sync reports imports, remote creations, pulls, pushes, and unresolved issues.
JSON mode writes the report to stdout; issues also cause a nonzero exit code.
Successful earlier tasks remain synced if a later task fails; retrying does
not recreate successfully mapped tasks.

### Conflicts, deletions, and limitations

If both copies changed since their last sync and differ, neither is overwritten.
Inspect the local task with `todo task show ID` and the remote task in ClickUp
(its ID appears in the issue report), then explicitly select the winning copy:

```sh
todo clickup sync 1 --prefer-local 42
todo clickup sync 1 --prefer-remote 42
```

These options resolve only the specified task; other conflicts stay unresolved.
The chosen copy wins for the supported fields, not ClickUp-only data such as
attachments, comments, tags, or custom fields.

- **Deletions never propagate.** Local deletions leave a mapping tombstone so
  the remote task is not reimported. Missing/deleted/inaccessible remote tasks
  are reported; local copies remain and are not recreated remotely automatically.
- Moving a linked task to another local project or another ClickUp List is
  reported, not mirrored automatically. Move it back to resume syncing.
- ClickUp's five priority levels are preserved exactly. Other unfinished
  statuses map to not-started locally; custom remote statuses are preserved
  unless you change task status locally.
- Existing timed start/due dates keep their remote time unless you change the
  respective date locally. Local edits intentionally write date-only values.
- Lists include completed tasks and subtasks in pagination; ClickUp tasks whose
  home List is elsewhere are not imported. Subtask relationships stay in ClickUp;
  newly created local tasks are top-level tasks.
- ClickUp has no conditional update/ETag support here. Sync checks for a remote
  change immediately before writing, but cannot eliminate a race with someone
  editing ClickUp during the write. Avoid concurrent edits while syncing.

If a create request times out or returns an uncertain result, `todo` remembers
the pending creation and **does not retry it blindly**. Further imports are
deferred until it is resolved to prevent duplicate tasks. Inspect the List:

```sh
# If the task exists remotely, attach its identity (differences become conflicts):
todo clickup recover 42 REMOTE_TASK_ID
# ONLY if you verified it was NOT created:
todo clickup retry-create 42 --confirm-missing
todo clickup sync 1
```

## Storage and separate workspaces

```sh
todo paths
todo --database ./planning.sqlite3
todo --database ./planning.sqlite3 --json task list
export TODO_DATABASE="$HOME/my-planning.sqlite3"
```

The default database lives in the platform's user-local data directory
(`~/.local/share/todo/` on typical Linux systems,
`~/Library/Application Support/todo/` on macOS). `todo paths` shows the exact
database and configuration paths. `TODO_DATABASE` or `--database` selects a
different workspace; no project files are created in your working directory
unless you explicitly choose a path there.

Data is local and unencrypted. Use normal OS file permissions to protect it.
Do not put the live SQLite database on a network drive or sync it between
machines while it is open. Separate databases are not shared lists. For a
simple backup, close all `todo` processes and copy the database file.

## Updates

Official release binaries embed their originating GitHub repository. Before
that repository is published, or for a local source build, configure it once:

```sh
todo update --repo OWNER/REPOSITORY --check
```

Alternatively, set `TODO_UPDATE_REPO=OWNER/REPOSITORY` or pass
`--update-repo OWNER/REPOSITORY`. Without a repository, no network requests are
made. Only public GitHub repositories/releases are supported currently.

The TUI checks the latest published stable release at startup using a short
network timeout. If an update exists, it asks before installing. Declining
opens the existing version; failed/offline checks never prevent task access.
Use `todo --no-update-check` or `TODO_NO_UPDATE_CHECK=true` to skip this.
Startup checks are intentionally limited to the interactive UI, not scripts.

```sh
todo update --check       # Only check
todo update               # Check and ask before installing
todo update --yes         # Explicitly permit installation without a prompt
```

Direct installs download the matching platform archive, verify its published
SHA-256 checksum, and atomically replace the executable. Restart afterward.
The install directory must be writable. Checksums detect corruption, not a
compromised release publisher; only configure a repository you trust.
Homebrew installations must update through Homebrew.

## Publishing releases

Push this project to a public GitHub repository and enable GitHub Actions.
For each release:

1. Set the version in `Cargo.toml` and update `Cargo.lock` with `cargo check`.
2. Commit and push the source and lockfile.
3. Tag that commit with the matching version and push the tag:

```sh
git tag v0.1.0
git push origin v0.1.0
```

`.github/workflows/release.yml` checks the tag against the Cargo version, runs
tests, builds four platform archives, produces their checksums and the Homebrew
formula, and publishes them as a GitHub Release. Release binaries automatically
know where to check for updates. Merely pushing source to GitHub does **not**
publish a new release. For the next update, increase the version and push a new tag.

Linux binaries target glibc-based distributions (Ubuntu 22.04/glibc 2.35 or
newer); Alpine/musl is not covered by the prebuilt releases. macOS releases
target Intel and Apple Silicon. ARM Linux release runners require a public
repository or a GitHub plan that supports those hosted runners.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

## Roadmap - later, not implemented

- Shared lists and collaborative read/write access across machines.
- FreeIPA-backed authentication/authorization for shared access.
- **Microsoft Planner integration.**

The current scope is local planning, automation-friendly CLI access, and optional
personal-token ClickUp sync. Shared-list hosting, FreeIPA, OAuth sign-in, and
Microsoft Planner are not implemented.
