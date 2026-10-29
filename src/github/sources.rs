use super::{projects, Api, Report};
use crate::store::Store;
use anyhow::{bail, Context, Result};
use rusqlite::params;
use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct Source {
    pub kind: String,
    pub remote_id: String,
    pub label: String,
    pub enabled: bool,
}

pub fn list(store: &Store) -> Result<Vec<Source>> {
    let mut stmt = store.conn.prepare(
        "SELECT kind,remote_id,label,enabled FROM github_sources ORDER BY kind,label COLLATE NOCASE",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Source {
            kind: r.get(0)?,
            remote_id: r.get(1)?,
            label: r.get(2)?,
            enabled: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn remember(
    store: &Store,
    kind: &str,
    id: &str,
    label: &str,
    project: Option<i64>,
) -> Result<()> {
    store.conn.execute(
        "INSERT INTO github_sources(kind,remote_id,label,project_id) VALUES(?1,?2,?3,?4)
         ON CONFLICT(kind,remote_id) DO UPDATE SET label=excluded.label,project_id=COALESCE(excluded.project_id,github_sources.project_id)",
        params![kind,id,label,project],
    )?;
    Ok(())
}

pub fn set_enabled(store: &Store, kind: &str, id: &str, enabled: bool) -> Result<()> {
    let tx = rusqlite::Transaction::new_unchecked(
        &store.conn,
        rusqlite::TransactionBehavior::Immediate,
    )?;
    if store.conn.execute(
        "UPDATE github_sources SET enabled=?1 WHERE kind=?2 AND remote_id=?3",
        params![enabled, kind, id],
    )? == 0
    {
        bail!("Unknown GitHub source; refresh the sources menu first");
    }
    if enabled {
        store
            .conn
            .execute("UPDATE github_preferences SET enabled=1", [])?;
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn selected(store: &Store, kind: &str, id: &str) -> Result<bool> {
    Ok(store.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM github_sources WHERE kind=?1 AND remote_id=?2 AND enabled=1)",
        params![kind, id],
        |r| r.get(0),
    )?)
}

pub(super) fn task_selected(store: &Store, id: i64) -> Result<bool> {
    Ok(store.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM github_issues g JOIN github_sources s
           ON s.kind='repo' AND s.remote_id=g.repository WHERE g.local_id=?1 AND s.enabled=1)
         OR EXISTS(SELECT 1 FROM github_project_items i JOIN github_projects p ON p.project_id=i.project_id
           JOIN github_sources s ON s.kind='project' AND s.remote_id=p.remote_id
           WHERE i.local_id=?1 AND i.active=1 AND s.enabled=1)",
        [id], |r| r.get(0),
    )?)
}

pub(super) fn refresh(store: &Store, api: &impl Api) -> Result<Report> {
    let profile = api.call("GET", "user", None)?;
    let login = profile["login"]
        .as_str()
        .context("GitHub profile has no login")?;
    let mut report = Report::default();
    let repos = (|| -> Result<()> {
        for page in 1..=1000 {
            let response = api.call(
                "GET",
                &format!("user/repos?per_page=100&sort=full_name&page={page}"),
                None,
            )?;
            let repos = response.as_array().context("Missing repository list")?;
            for repo in repos {
                let name = repo["full_name"]
                    .as_str()
                    .context("Repository has no name")?;
                remember(store, "repo", name, name, None)?;
            }
            if repos.len() < 100 {
                return Ok(());
            }
        }
        bail!("Repository pagination limit reached");
    })();
    if let Err(error) = repos {
        report
            .issues
            .push(format!("GitHub repositories: {error:#}"));
    }
    projects::discover(store, api, login, &mut report, true)?;
    Ok(report)
}
