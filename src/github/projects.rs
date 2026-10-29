use super::{import_issue, sources, Api, Issue, Report, User};
use crate::store::{Assignee, Priority, Store};
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct Bucket {
    pub item: String,
    pub project: String,
    pub field: String,
    pub option: Option<String>,
    pub baseline: Option<String>,
    pub label: String,
    pub status: Option<String>,
}

fn current_status(api: &impl Api, item: &str) -> Result<Option<String>> {
    let data = graphql(api,
        "query($id:ID!){node(id:$id){... on ProjectV2Item{fieldValueByName(name:\"Status\"){... on ProjectV2ItemFieldSingleSelectValue{optionId}}}}}",
        json!({"id":item}))?;
    if data["node"].is_null() {
        bail!("GitHub Project item is no longer accessible");
    }
    Ok(data["node"]["fieldValueByName"]["optionId"]
        .as_str()
        .map(str::to_owned))
}

fn check_membership(store: &Store, item: &str, project: &str) -> Result<()> {
    let available: bool = store.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM github_project_items i JOIN github_projects p ON p.project_id=i.project_id
         JOIN github_sources s ON s.kind='project' AND s.remote_id=p.remote_id
         WHERE i.remote_id=?1 AND p.remote_id=?2 AND i.active=1 AND i.local_id IS NOT NULL AND s.enabled=1)",
        params![item,project], |r| r.get(0)
    )?;
    if !available {
        bail!("Select this GitHub Project source first, or refresh its membership");
    }
    Ok(())
}

pub(super) fn buckets(
    store: &Store,
    api: &impl Api,
    task: i64,
    scope: crate::store::Scope,
) -> Result<Vec<Bucket>> {
    let project = match scope {
        crate::store::Scope::Project(id) => Some(id),
        _ => None,
    };
    let mut stmt = store.conn.prepare(
        "SELECT i.remote_id,p.remote_id,p.owner || ' / ' || p.title FROM github_project_items i
         JOIN github_projects p ON p.project_id=i.project_id JOIN github_sources s ON s.kind='project' AND s.remote_id=p.remote_id
         WHERE i.local_id=?1 AND i.active=1 AND s.enabled=1
           AND (?2 IS NULL OR NOT EXISTS(SELECT 1 FROM github_projects WHERE project_id=?2) OR i.project_id=?2)
         ORDER BY p.owner,p.title"
    )?;
    let memberships = stmt
        .query_map(params![task, project], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut buckets = Vec::new();
    for (item, project, label) in memberships {
        let baseline = current_status(api, &item)?;
        let mut cursor = None::<String>;
        loop {
            let data = graphql(api,
                "query($id:ID!,$cursor:String){node(id:$id){... on ProjectV2{fields(first:100,after:$cursor){nodes{... on ProjectV2SingleSelectField{id name options{id name}}} pageInfo{hasNextPage endCursor}}}}}",
                json!({"id":project,"cursor":cursor}))?;
            let connection = &data["node"]["fields"];
            for field in connection["nodes"]
                .as_array()
                .context("Missing Project fields")?
            {
                if field["name"].as_str() != Some("Status") {
                    continue;
                }
                let field_id = field["id"].as_str().context("Missing Status field ID")?;
                for option in field["options"]
                    .as_array()
                    .context("Missing Project buckets")?
                {
                    let id = option["id"]
                        .as_str()
                        .context("Bucket has no ID")?
                        .to_owned();
                    let name = option["name"]
                        .as_str()
                        .context("Bucket has no name")?
                        .to_owned();
                    buckets.push(Bucket {
                        item: item.clone(),
                        project: project.clone(),
                        field: field_id.into(),
                        option: Some(id),
                        baseline: baseline.clone(),
                        label: format!("{label} / {name}"),
                        status: Some(name),
                    });
                }
                buckets.push(Bucket {
                    item: item.clone(),
                    project: project.clone(),
                    field: field_id.into(),
                    option: None,
                    baseline: baseline.clone(),
                    label: format!("{label} / No status"),
                    status: None,
                });
            }
            let next_cursor = next(connection)?;
            if next_cursor.is_some() && next_cursor == cursor {
                bail!("GitHub repeated a Project field cursor");
            }
            cursor = next_cursor;
            if cursor.is_none() {
                break;
            }
        }
    }
    if buckets.is_empty() {
        bail!("No writable Status buckets found. Enable and sync the item's GitHub Project under ? → GitHub sources first");
    }
    Ok(buckets)
}

pub(super) fn move_bucket(store: &Store, api: &impl Api, bucket: &Bucket) -> Result<()> {
    check_membership(store, &bucket.item, &bucket.project)?;
    let current = current_status(api, &bucket.item)?;
    if current != bucket.option {
        if current != bucket.baseline {
            bail!("Project status changed on GitHub; reopen the bucket picker before moving");
        }
        check_membership(store, &bucket.item, &bucket.project)?;
        let (query, variables) = if let Some(option) = &bucket.option {
            ("mutation($project:ID!,$item:ID!,$field:ID!,$option:String!){updateProjectV2ItemFieldValue(input:{projectId:$project,itemId:$item,fieldId:$field,value:{singleSelectOptionId:$option}}){projectV2Item{id}}}",
             json!({"project":bucket.project,"item":bucket.item,"field":bucket.field,"option":option}))
        } else {
            ("mutation($project:ID!,$item:ID!,$field:ID!){clearProjectV2ItemFieldValue(input:{projectId:$project,itemId:$item,fieldId:$field}){projectV2Item{id}}}",
             json!({"project":bucket.project,"item":bucket.item,"field":bucket.field}))
        };
        let data = graphql(api,query,variables).context(
            "Cannot move Project item; check Project write access and authorize `gh auth refresh --hostname github.com --scopes project`"
        )?;
        let operation = if bucket.option.is_some() {
            "updateProjectV2ItemFieldValue"
        } else {
            "clearProjectV2ItemFieldValue"
        };
        if data[operation]["projectV2Item"]["id"].as_str() != Some(&bucket.item) {
            bail!("GitHub did not confirm the Project move; refresh before retrying");
        }
        if current_status(api, &bucket.item)? != bucket.option {
            bail!("GitHub Project status changed during the move; refresh before retrying");
        }
    }
    store.conn.execute(
        "UPDATE github_project_items SET status=?1 WHERE remote_id=?2",
        params![bucket.status, bucket.item],
    )?;
    Ok(())
}

fn graphql(api: &impl Api, query: &str, variables: Value) -> Result<Value> {
    let response = api.call(
        "POST",
        "graphql",
        Some(json!({"query":query,"variables":variables})),
    ).context("GitHub Projects request failed; grant read:project using `gh auth refresh --hostname github.com --scopes read:project` and check organization SSO")?;
    if response
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        bail!("GitHub Projects access failed; grant read:project using `gh auth refresh --hostname github.com --scopes read:project` and check organization SSO");
    }
    response
        .get("data")
        .cloned()
        .context("GitHub GraphQL response is missing data")
}

fn next(connection: &Value) -> Result<Option<String>> {
    let page = connection
        .get("pageInfo")
        .context("Missing GitHub pagination information")?;
    if page["hasNextPage"]
        .as_bool()
        .context("Missing GitHub pagination flag")?
    {
        Ok(Some(
            page["endCursor"]
                .as_str()
                .context("Missing GitHub pagination cursor")?
                .to_owned(),
        ))
    } else {
        Ok(None)
    }
}

pub(super) fn discover(
    store: &Store,
    api: &impl Api,
    login: &str,
    report: &mut Report,
    catalog_only: bool,
) -> Result<()> {
    if !catalog_only {
        let mut stmt = store.conn.prepare("SELECT p.owner,p.remote_id,p.title,p.url FROM github_projects p
            JOIN github_sources s ON s.kind='project' AND s.remote_id=p.remote_id WHERE s.enabled=1")?;
        let projects = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (owner, id, title, url) in projects {
            if let Err(error) = import_project(
                store,
                api,
                &owner,
                login,
                &json!({"id":id,"title":title,"url":url}),
                report,
                false,
            ) {
                report
                    .issues
                    .push(format!("GitHub project {owner}/{title}: {error:#}"));
            }
        }
        return Ok(());
    }
    let mut owners = vec![(login.to_owned(), "user")];
    let organizations = (|| -> Result<()> {
        let mut cursor = None::<String>;
        loop {
            let data = graphql(api, "query($cursor:String){viewer{organizations(first:100,after:$cursor){nodes{login} pageInfo{hasNextPage endCursor}}}}", json!({"cursor":cursor}))?;
            let connection = &data["viewer"]["organizations"];
            for org in connection["nodes"]
                .as_array()
                .context("Missing organization list")?
            {
                owners.push((
                    org["login"]
                        .as_str()
                        .context("Organization has no login")?
                        .to_owned(),
                    "organization",
                ));
            }
            cursor = next(connection)?;
            if cursor.is_none() {
                break;
            }
        }
        Ok(())
    })();
    if let Err(error) = organizations {
        report
            .issues
            .push(format!("GitHub organizations: {error:#}"));
    }
    for (owner, kind) in owners {
        let result = (|| -> Result<()> {
            let query = format!("query($owner:String!,$cursor:String){{{kind}(login:$owner){{projectsV2(first:100,after:$cursor){{nodes{{id title url}} pageInfo{{hasNextPage endCursor}}}}}}}}");
            let mut cursor = None::<String>;
            loop {
                let data = graphql(api, &query, json!({"owner":owner,"cursor":cursor}))?;
                let connection = &data[kind]["projectsV2"];
                for project in connection["nodes"]
                    .as_array()
                    .context("Missing Projects list")?
                {
                    if let Err(error) =
                        import_project(store, api, &owner, login, project, report, true)
                    {
                        report.issues.push(format!(
                            "GitHub project {owner} / {}: {error:#}",
                            project["title"].as_str().unwrap_or("?")
                        ));
                    }
                }
                cursor = next(connection)?;
                if cursor.is_none() {
                    break;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            report
                .issues
                .push(format!("GitHub Projects for {owner}: {error:#}"));
        }
    }
    Ok(())
}

const ITEMS: &str = r#"query($id:ID!,$cursor:String){
 node(id:$id){... on ProjectV2{
  items(first:100,after:$cursor){
   nodes{id content{
    __typename
    ... on Issue{id number title body state closedAt url repository{nameWithOwner} assignees(first:100){nodes{databaseId login}}}
    ... on PullRequest{id number title body state closedAt url repository{nameWithOwner} assignees(first:100){nodes{databaseId login}}}
    ... on DraftIssue{id title body assignees(first:100){nodes{databaseId login}}}
   }
   fieldValues(first:100){nodes{
    ... on ProjectV2ItemFieldSingleSelectValue{name field{... on ProjectV2SingleSelectField{name}}}
    ... on ProjectV2ItemFieldDateValue{date field{... on ProjectV2Field{name}}}
   }}}
   pageInfo{hasNextPage endCursor}
  }
 }}
}"#;

fn import_project(
    store: &Store,
    api: &impl Api,
    owner: &str,
    login: &str,
    project: &Value,
    report: &mut Report,
    catalog_only: bool,
) -> Result<()> {
    let remote = project["id"].as_str().context("Project has no ID")?;
    let title = project["title"].as_str().context("Project has no title")?;
    let url = project["url"]
        .as_str()
        .filter(|url| url.starts_with("https://github.com/"))
        .context("Invalid Project URL")?;
    let project_id = {
        let tx = rusqlite::Transaction::new_unchecked(
            &store.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let existing = store
            .conn
            .query_row(
                "SELECT project_id FROM github_projects WHERE remote_id=?1",
                [remote],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        let id = if let Some(id) = existing {
            id
        } else {
            let base = format!("GitHub Project: {owner} / {title}");
            let mut name = base.clone();
            let mut suffix = 1;
            while store
                .projects()?
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case(&name))
            {
                name = format!("{base} ({suffix})");
                suffix += 1;
            }
            let id = store.add_project(&name)?.id;
            store.conn.execute("INSERT INTO github_projects(project_id,remote_id,owner,title,url) VALUES(?1,?2,?3,?4,?5)",params![id,remote,owner,title,url])?;
            id
        };
        store.conn.execute(
            "UPDATE github_projects SET owner=?1,title=?2,url=?3 WHERE project_id=?4",
            params![owner, title, url, id],
        )?;
        sources::remember(
            store,
            "project",
            remote,
            &format!("{owner} / {title}"),
            Some(id),
        )?;
        tx.commit()?;
        id
    };
    if catalog_only {
        return Ok(());
    }
    let mut cursor = None::<String>;
    let mut seen = Vec::new();
    loop {
        if !sources::selected(store, "project", remote)? {
            return Ok(());
        }
        let data = graphql(api, ITEMS, json!({"id":remote,"cursor":cursor}))?;
        let connection = &data["node"]["items"];
        for item in connection["nodes"]
            .as_array()
            .context("Missing project items")?
        {
            if let Some(id) = item["id"].as_str() {
                seen.push(id.to_owned());
            }
            match import_item(store, project_id, login, item) {
                Ok(created) => report.imported += usize::from(created),
                Err(error) => report
                    .issues
                    .push(format!("GitHub project {owner}/{title} item: {error:#}")),
            }
        }
        cursor = next(connection)?;
        if cursor.is_none() {
            break;
        }
    }
    // Keep identity and tombstones even after an item leaves the project.
    let mut stmt = store
        .conn
        .prepare("SELECT remote_id FROM github_project_items WHERE project_id=?1")?;
    let old = stmt
        .query_map([project_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in old {
        if !seen.contains(&id) {
            store.conn.execute(
                "UPDATE github_project_items SET active=0,assigned=0 WHERE remote_id=?1",
                [id],
            )?;
        }
    }
    Ok(())
}

fn import_item(store: &Store, project: i64, login: &str, item: &Value) -> Result<bool> {
    let id = item["id"].as_str().context("Item has no ID")?;
    let content = &item["content"];
    if content.is_null() {
        bail!("Item content is inaccessible or deleted; local data retained");
    }
    let content_id = content["id"].as_str().context("Item has no content ID")?;
    let kind = content["__typename"]
        .as_str()
        .context("Unknown project item type")?;
    let users = content["assignees"]["nodes"]
        .as_array()
        .context("Missing assignees")?;
    let assignees: Vec<Assignee> = users
        .iter()
        .map(|u| {
            Ok(Assignee {
                id: u["databaseId"]
                    .as_i64()
                    .context("GitHub user has no numeric ID")?,
                username: Some(
                    u["login"]
                        .as_str()
                        .context("GitHub user has no login")?
                        .to_owned(),
                ),
            })
        })
        .collect::<Result<_>>()?;
    let assigned = assignees.iter().any(|u| {
        u.username
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(login))
    });
    let tx = if kind == "Issue" {
        None
    } else {
        Some(rusqlite::Transaction::new_unchecked(
            &store.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?)
    };
    let (local, created) = if kind == "Issue" {
        let repository = content["repository"]["nameWithOwner"]
            .as_str()
            .context("Issue has no repository")?;
        let issue = Issue {
            number: content["number"].as_i64().context("Issue has no number")?,
            title: content["title"]
                .as_str()
                .context("Issue has no title")?
                .to_owned(),
            body: content["body"].as_str().map(str::to_owned),
            state: content["state"]
                .as_str()
                .context("Issue has no state")?
                .to_ascii_lowercase(),
            html_url: content["url"]
                .as_str()
                .context("Issue has no URL")?
                .to_owned(),
            repository_url: format!("https://api.github.com/repos/{repository}"),
            closed_at: content["closedAt"].as_str().map(str::to_owned),
            assignees: assignees
                .iter()
                .map(|u| User {
                    id: u.id,
                    login: u.username.clone().unwrap(),
                })
                .collect(),
        };
        import_issue(store, &issue)?
    } else if matches!(kind, "PullRequest" | "DraftIssue") {
        let existing = store
            .conn
            .query_row(
                "SELECT local_id FROM github_project_items WHERE content_id=?1 LIMIT 1",
                [content_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?;
        if let Some(local) = existing {
            (local, false)
        } else {
            let task = store.add_task(
                content["title"].as_str().context("Item has no title")?,
                content["body"].as_str().unwrap_or(""),
                Some(project),
                Priority::Normal,
                None,
            )?;
            (Some(task.id), true)
        }
    } else {
        bail!("Unsupported project item type {kind}");
    };
    if kind != "Issue" {
        if let Some(local) = local {
            store.conn.execute(
                "UPDATE tasks SET done=?1,completed_at=?2,assignees=?3 WHERE id=?4",
                params![
                    kind == "PullRequest" && content["state"].as_str() != Some("OPEN"),
                    content["closedAt"].as_str(),
                    serde_json::to_string(&assignees)?,
                    local
                ],
            )?;
        }
    }
    let mut status = None::<String>;
    let mut fields = Vec::new();
    for field in item["fieldValues"]["nodes"]
        .as_array()
        .context("Missing project fields")?
    {
        let name = field["field"]["name"].as_str().unwrap_or("");
        let value = field["date"].as_str().or_else(|| field["name"].as_str());
        if let Some(value) = value {
            if name.eq_ignore_ascii_case("status") {
                status = Some(value.to_owned());
            } else if !name.is_empty() {
                fields.push(format!("{name}: {value}"));
            }
        }
    }
    store.conn.execute("INSERT INTO github_project_items(remote_id,project_id,local_id,kind,content_id,status,fields,assigned) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
        ON CONFLICT(remote_id) DO UPDATE SET project_id=excluded.project_id,status=excluded.status,fields=excluded.fields,assigned=excluded.assigned,active=1",
        params![id,project,local,kind,content_id,status,fields.join(" | "),assigned])?;
    if let Some(tx) = tx {
        tx.commit()?;
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Scope, Status};
    use std::cell::{Cell, RefCell};

    struct BucketApi {
        statuses: RefCell<std::collections::HashMap<String, Option<String>>>,
        writes: RefCell<Vec<Value>>,
        denied: Cell<bool>,
    }

    impl Api for BucketApi {
        fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
            assert_eq!((method, path), ("POST", "graphql"));
            let body = body.unwrap();
            let query = body["query"].as_str().unwrap();
            let vars = &body["variables"];
            if query.contains("fieldValueByName") {
                let id = vars["id"].as_str().unwrap();
                return Ok(
                    json!({"data":{"node":{"fieldValueByName":self.statuses.borrow()[id].as_ref().map(|id|json!({"optionId":id}))}}}),
                );
            }
            if query.contains("fields(") {
                let nodes = if vars["cursor"].is_null() {
                    vec![]
                } else {
                    vec![
                        json!({"id":"STATUS","name":"Status","options":[{"id":"TODO","name":"Todo"},{"id":"DONE","name":"Done"}]}),
                    ]
                };
                return Ok(
                    json!({"data":{"node":{"fields":connection(nodes,vars["cursor"].is_null().then_some("next"))}}}),
                );
            }
            assert!(query.starts_with("mutation"));
            self.writes.borrow_mut().push(body.clone());
            if self.denied.get() {
                return Ok(json!({"errors":[{"message":"Forbidden"}]}));
            }
            let option = vars["option"].as_str().map(str::to_owned);
            self.statuses
                .borrow_mut()
                .insert(vars["item"].as_str().unwrap().into(), option);
            let operation = if query.contains("clearProject") {
                "clearProjectV2ItemFieldValue"
            } else {
                "updateProjectV2ItemFieldValue"
            };
            Ok(json!({"data":{operation:{"projectV2Item":{"id":vars["item"]}}}}))
        }
    }

    #[test]
    fn bucket_moves_are_project_specific_preserve_issue_state_and_handle_clear_conflict_and_denial()
    {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        let discovery = Mock {
            calls: RefCell::new(vec![]),
            denied: false,
            empty: Cell::new(false),
            fail_page: Cell::new(false),
        };
        discover(&store, &discovery, "sam", &mut Report::default(), true).unwrap();
        for id in ["P1", "P2"] {
            sources::set_enabled(&store, "project", id, true).unwrap();
        }
        discover(&store, &discovery, "sam", &mut Report::default(), false).unwrap();
        let task = store
            .tasks(Scope::All, Status::All, "")
            .unwrap()
            .into_iter()
            .find(|t| t.github_url.is_some())
            .unwrap();
        assert_eq!(
            store.github_buckets(Scope::All).unwrap()[&task.id],
            "Done / Done"
        );
        let api = BucketApi {
            statuses: RefCell::new(
                [
                    ("I1".into(), Some("TODO".into())),
                    ("I2".into(), Some("TODO".into())),
                    ("D1".into(), Some("TODO".into())),
                ]
                .into(),
            ),
            writes: RefCell::new(vec![]),
            denied: Cell::new(false),
        };
        let options = buckets(&store, &api, task.id, Scope::All).unwrap();
        assert_eq!(options.len(), 6);
        let done = options
            .iter()
            .find(|b| b.project == "P1" && b.option.as_deref() == Some("DONE"))
            .unwrap();
        move_bucket(&store, &api, done).unwrap();
        assert!(
            !store.task(task.id).unwrap().done,
            "Moving to Done must not close the issue"
        );
        assert_eq!(api.statuses.borrow()["I2"], Some("TODO".into()));
        assert_eq!(api.writes.borrow()[0]["variables"]["project"], "P1");
        assert!(store
            .github_context(task.id, Scope::All)
            .unwrap()
            .contains("Personal project: Done"));
        let project_id: i64 = store
            .conn
            .query_row(
                "SELECT project_id FROM github_projects WHERE remote_id='P1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let scoped = buckets(&store, &api, task.id, Scope::Project(project_id)).unwrap();
        assert_eq!(scoped.len(), 3);
        assert!(scoped.iter().all(|b| b.project == "P1"));
        let clear = scoped.iter().find(|b| b.option.is_none()).unwrap();
        move_bucket(&store, &api, clear).unwrap();
        assert_eq!(
            store.github_buckets(Scope::Project(project_id)).unwrap()[&task.id],
            "No status"
        );
        assert!(api.statuses.borrow()["I1"].is_none());
        assert!(api.writes.borrow()[1]["query"]
            .as_str()
            .unwrap()
            .contains("clearProjectV2ItemFieldValue"));
        let options = buckets(&store, &api, task.id, Scope::All).unwrap();
        let done = options
            .iter()
            .find(|b| b.project == "P1" && b.option.as_deref() == Some("DONE"))
            .unwrap();
        api.statuses
            .borrow_mut()
            .insert("I1".into(), Some("TODO".into()));
        assert!(move_bucket(&store, &api, done)
            .unwrap_err()
            .to_string()
            .contains("status changed"));
        assert_eq!(api.writes.borrow().len(), 2);
        let options = buckets(&store, &api, task.id, Scope::All).unwrap();
        let done = options
            .iter()
            .find(|b| b.project == "P1" && b.option.as_deref() == Some("DONE"))
            .unwrap();
        api.denied.set(true);
        let error = move_bucket(&store, &api, done).unwrap_err();
        assert!(format!("{error:#}").contains("--scopes project"));
        let cached: Option<String> = store
            .conn
            .query_row(
                "SELECT status FROM github_project_items WHERE remote_id='I1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(cached.is_none(), "A denied move must not update the cache");
        api.denied.set(false);
        sources::set_enabled(&store, "project", "P1", false).unwrap();
        let writes = api.writes.borrow().len();
        assert!(move_bucket(&store, &api, done).is_err());
        assert_eq!(api.writes.borrow().len(), writes);
        sources::set_enabled(&store, "project", "P1", true).unwrap();
        let draft = store
            .tasks(Scope::All, Status::All, "")
            .unwrap()
            .into_iter()
            .find(|t| t.title == "DraftIssue title")
            .unwrap();
        let draft_options = buckets(&store, &api, draft.id, Scope::All).unwrap();
        move_bucket(&store, &api, &draft_options[1]).unwrap();
        assert!(store.task(draft.id).unwrap().github_read_only);
    }

    struct Mock {
        calls: RefCell<Vec<Value>>,
        denied: bool,
        empty: Cell<bool>,
        fail_page: Cell<bool>,
    }

    fn connection(nodes: Vec<Value>, cursor: Option<&str>) -> Value {
        json!({"nodes":nodes,"pageInfo":{"hasNextPage":cursor.is_some(),"endCursor":cursor}})
    }

    fn item(kind: &str, id: &str, content_id: &str) -> Value {
        json!({"id":id,"content":{"__typename":kind,"id":content_id,"number":1,"title":format!("{kind} title"),"body":"Body","state":"OPEN","closedAt":null,
            "url":"https://github.com/acme/repo/issues/1","repository":{"nameWithOwner":"acme/repo"},
            "assignees":{"nodes":[{"databaseId":7,"login":"sam"}]}},
            "fieldValues":{"nodes":[{"name":"Done","field":{"name":"Status"}},{"date":"2026-12-01","field":{"name":"Finish"}}]}})
    }

    impl Api for Mock {
        fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
            assert_eq!(method, "POST");
            assert_eq!(path, "graphql");
            let body = body.unwrap();
            self.calls.borrow_mut().push(body.clone());
            let query = body["query"].as_str().unwrap();
            let vars = &body["variables"];
            if query.contains("organizations(") {
                if vars["cursor"].is_null() {
                    return Ok(
                        json!({"data":{"viewer":{"organizations":connection(vec![json!({"login":"acme"})],Some("org-next"))}}}),
                    );
                }
                return Ok(
                    json!({"data":{"viewer":{"organizations":connection(vec![json!({"login":"denied"})],None)}}}),
                );
            }
            if vars["owner"] == "denied" {
                if self.denied {
                    return Ok(json!({"errors":[{"message":"Forbidden"}]}));
                }
                return Ok(json!({"data":{"organization":{"projectsV2":connection(vec![],None)}}}));
            }
            if query.contains("projectsV2(") {
                let owner = vars["owner"].as_str().unwrap();
                let node = if owner == "sam" {
                    json!({"id":"P1","title":"Personal project","url":"https://github.com/users/sam/projects/1"})
                } else {
                    json!({"id":"P2","title":"Org project","url":"https://github.com/orgs/acme/projects/2"})
                };
                let kind = if owner == "sam" {
                    "user"
                } else {
                    "organization"
                };
                let conn = if vars["cursor"].is_null() {
                    connection(vec![node], Some("project-next"))
                } else {
                    connection(vec![], None)
                };
                return Ok(json!({"data":{kind:{"projectsV2":conn}}}));
            }
            let personal = vars["id"] == "P1";
            if self.fail_page.get() && !vars["cursor"].is_null() {
                bail!("Later project page unavailable");
            }
            let conn = if self.empty.get() {
                connection(vec![], None)
            } else if vars["cursor"].is_null() {
                connection(
                    vec![item("Issue", if personal { "I1" } else { "I2" }, "issue1")],
                    Some("item-next"),
                )
            } else if personal {
                connection(
                    vec![
                        item("DraftIssue", "D1", "draft1"),
                        item("PullRequest", "R1", "pr1"),
                    ],
                    None,
                )
            } else {
                connection(vec![], None)
            };
            Ok(json!({"data":{"node":{"items":conn}}}))
        }
    }

    #[test]
    fn discovers_paginated_user_org_projects_and_shared_items_without_duplicate_issues() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        let api = Mock {
            calls: RefCell::new(vec![]),
            denied: true,
            empty: Cell::new(false),
            fail_page: Cell::new(false),
        };
        let mut report = Report::default();
        discover(&store, &api, "sam", &mut report, true).unwrap();
        assert_eq!(report.imported, 0);
        assert!(store.visible_projects().unwrap().is_empty());
        for id in ["P1", "P2"] {
            sources::set_enabled(&store, "project", id, true).unwrap();
        }
        discover(&store, &api, "sam", &mut report, false).unwrap();
        assert_eq!(report.imported, 3);
        assert_eq!(report.issues.len(), 1);
        assert!(report.issues[0].contains("read:project"));
        let mut stmt = store
            .conn
            .prepare("SELECT project_id FROM github_projects ORDER BY remote_id")
            .unwrap();
        let ids = stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(ids.len(), 2);
        assert!(store.is_github_project(ids[0]).unwrap());
        assert_eq!(
            store
                .tasks(Scope::Project(ids[0]), Status::All, "")
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            store
                .tasks(Scope::Project(ids[1]), Status::All, "")
                .unwrap()
                .len(),
            1
        );
        let tasks = store.tasks(Scope::All, Status::All, "").unwrap();
        assert_eq!(tasks.len(), 3);
        let issue = tasks.iter().find(|t| t.github_url.is_some()).unwrap();
        assert!(!issue.done, "Done project status must not close an issue");
        assert!(store
            .github_context(issue.id, Scope::All)
            .unwrap()
            .contains("Org project: Done"));
        assert!(store
            .github_context(issue.id, Scope::All)
            .unwrap()
            .contains("Finish: 2026-12-01"));
        assert_eq!(tasks.iter().filter(|t| t.github_read_only).count(), 2);
        let draft = tasks
            .iter()
            .find(|t| t.title == "DraftIssue title")
            .unwrap();
        assert!(store.set_done(draft.id, true).is_err());
        store
            .conn
            .execute("UPDATE github_preferences SET enabled=1", [])
            .unwrap();
        let mut assigned = tasks.clone();
        store.assigned_to_me(&mut assigned).unwrap();
        assert_eq!(assigned.len(), 3);
        let mut report = Report::default();
        discover(&store, &api, "sam", &mut report, false).unwrap();
        assert_eq!(report.imported, 0);
        sources::set_enabled(&store, "project", "P1", false).unwrap();
        let visible = store.tasks(Scope::All, Status::All, "").unwrap();
        assert_eq!(
            visible.len(),
            1,
            "The shared issue stays visible through P2"
        );
        assert_eq!(visible[0].id, issue.id);
        assert!(store
            .tasks(Scope::Project(ids[0]), Status::All, "")
            .unwrap()
            .is_empty());
        api.calls.borrow_mut().clear();
        discover(&store, &api, "sam", &mut report, false).unwrap();
        assert!(api
            .calls
            .borrow()
            .iter()
            .all(|call| call["variables"]["id"] == "P2"));
        sources::set_enabled(&store, "project", "P1", true).unwrap();
        assert_eq!(store.tasks(Scope::All, Status::All, "").unwrap().len(), 3);
        api.empty.set(true);
        discover(&store, &api, "sam", &mut report, false).unwrap();
        assert!(store
            .tasks(Scope::Project(ids[0]), Status::All, "")
            .unwrap()
            .is_empty());
        assert!(store.tasks(Scope::All, Status::All, "").unwrap().is_empty());
        assert!(store.task(draft.id).unwrap().github_read_only);
    }

    #[test]
    fn failed_later_pages_preserve_memberships_and_deleted_items_are_not_reimported() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        let api = Mock {
            calls: RefCell::new(vec![]),
            denied: false,
            empty: Cell::new(false),
            fail_page: Cell::new(false),
        };
        discover(&store, &api, "sam", &mut Report::default(), true).unwrap();
        for id in ["P1", "P2"] {
            sources::set_enabled(&store, "project", id, true).unwrap();
        }
        discover(&store, &api, "sam", &mut Report::default(), false).unwrap();
        api.fail_page.set(true);
        let mut report = Report::default();
        discover(&store, &api, "sam", &mut report, false).unwrap();
        assert_eq!(report.issues.len(), 2);
        let active: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM github_project_items WHERE active=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            active, 4,
            "A failed page must not remove cached memberships"
        );
        for task in store.tasks(Scope::All, Status::All, "").unwrap() {
            if task.github_url.is_some() || task.title == "DraftIssue title" {
                store.delete_task(task.id).unwrap();
            }
        }
        api.fail_page.set(false);
        let mut report = Report::default();
        discover(&store, &api, "sam", &mut report, false).unwrap();
        assert_eq!(report.imported, 0);
        let tasks = store.tasks(Scope::All, Status::All, "").unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "PullRequest title");
    }
}
