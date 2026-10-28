//! The board as MCP tools: over stdio for `kanzo mcp`, and over HTTP at `/mcp` of the board.
//! Replies are short plain text to save agents' tokens.

use std::collections::HashMap;
use std::fmt::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, Peer, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::home::Home;
use crate::store::{self, Error, Project, Store, Task, TaskPatch, Updated, short_time};

/// The most done tasks list_tasks shows, newest first: an old board has thousands.
const DONE_LISTED: usize = 20;

const INSTRUCTIONS: &str = "The user's kanban board, on this machine, for all their projects. Tasks look like \
api#12. Without `project`, Kanzo uses the project linked to this folder's GitHub repo. Keep it current unasked: \
doing when you start a task, comments on what you did, review when you finish or get stuck. Leave done and \
human tasks to the user.";

const HTTP_INSTRUCTIONS: &str = "The user's kanban board, on their machine, for all their projects. Tasks look \
like api#12. Over HTTP Kanzo can't see your folder, so pass `project` (see list_projects). Keep it current \
unasked: doing when you start a task, comments on what you did, review when you finish or get stuck. Leave \
done and human tasks to the user.";

/// `kanzo mcp`: one agent on stdin and stdout, in the folder it started us in.
pub async fn serve(home: Home) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let repos = git_repos(&cwd);
    let ctx = Arc::new(Ctx { home, cwd: Some(cwd), repos });
    let server = Kanzo { ctx, tool_router: Kanzo::tools(), seen: Seen::default() };
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// MCP over HTTP, mounted at `/mcp` by the board. The board's own Host and Origin
/// checks (`web::protect`) guard it, including when it listens on the network, so
/// rmcp's localhost-only list is turned off here.
pub fn http_service(home: Home, shutdown: CancellationToken) -> StreamableHttpService<Kanzo, LocalSessionManager> {
    let ctx = Arc::new(Ctx { home, cwd: None, repos: Vec::new() });
    let config = StreamableHttpServerConfig::default().disable_allowed_hosts().with_cancellation_token(shutdown);
    // rmcp makes a server per session, and for some requests; the tool list is built once.
    let tool_router = Kanzo::tools();
    // ponytail: rmcp calls this once per session for clients on MCP before 2026-07-28, so
    // `seen` lasts the session. From 2026-07-28 on it serves each request on its own server,
    // `seen` starts empty, and update_task overwrites as it did before conflict copies.
    StreamableHttpService::new(
        move || Ok(Kanzo { ctx: ctx.clone(), tool_router: tool_router.clone(), seen: Seen::default() }),
        Default::default(),
        config,
    )
}

struct Ctx {
    home: Home,
    /// The agent's working folder; `None` over HTTP, where we cannot know it.
    cwd: Option<PathBuf>,
    /// GitHub repos (`owner/repo`) of the git remotes of `cwd`.
    repos: Vec<String>,
}

impl Ctx {
    /// The named project, or the one linked to this folder's git remote.
    fn project(&self, store: &Store, name: Option<&str>) -> store::Result<Project> {
        if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
            return store.find_project(name);
        }
        for repo in &self.repos {
            if let Some(p) = store.project_by_repo(repo)? {
                return Ok(p);
            }
        }
        let known: Vec<String> = store.list_projects()?.into_iter().map(|p| p.slug).collect();
        let known = known.join(", ");
        let Some(cwd) = &self.cwd else {
            let msg = if known.is_empty() {
                "There are no projects yet. Create one with create_project.".to_string()
            } else {
                format!("Pass `project` (one of: {known}); over HTTP Kanzo cannot see which folder you work in.")
            };
            return Err(Error::NotFound(msg));
        };
        let cwd = cwd.display();
        let msg = match (self.repos.first(), known.is_empty()) {
            (Some(repo), false) => format!(
                "No project is linked to {repo} (the git remote of {cwd}). Pass `project` (one of: {known}), \
                 link this repo to one with link_repo, or create a project for it with create_project and repo \"{repo}\"."
            ),
            (Some(repo), true) => format!(
                "No project is linked to {repo} (the git remote of {cwd}), and there are no projects yet. \
                 Create one with create_project and repo \"{repo}\"."
            ),
            (None, false) => {
                format!("Cannot tell the project: {cwd} has no git remote. Pass `project` (one of: {known}).")
            }
            (None, true) => "There are no projects yet. Create one with create_project.".to_string(),
        };
        Err(Error::NotFound(msg))
    }
}

fn git_repos(dir: &std::path::Path) -> Vec<String> {
    let Ok(out) = Command::new("git").arg("-C").arg(dir).args(["remote", "-v"]).output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    // `origin` first, so it wins when several remotes are linked to different projects.
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort_by_key(|l| !l.starts_with("origin\t"));
    let mut repos = Vec::new();
    for line in lines {
        if let Some(repo) = line.split_whitespace().nth(1).and_then(store::repo_from_url)
            && !repos.contains(&repo)
        {
            repos.push(repo);
        }
    }
    repos
}

#[derive(Clone)]
pub struct Kanzo {
    ctx: Arc<Ctx>,
    tool_router: ToolRouter<Self>,
    seen: Seen,
}

/// The last version of each task this session showed its agent, so update_task can tell
/// when the agent edits over a change it hasn't read.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<HashMap<i64, Task>>>);

impl Seen {
    fn saw(&self, t: &Task) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(t.id, t.clone());
    }

    /// `saw`, for a reply that showed or wrote the description or the tags only when
    /// `description` or `tags`: the others stay as last shown.
    fn saw_some(&self, t: &Task, description: bool, tags: bool) {
        let mut seen = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut now = t.clone();
        if let Some(old) = seen.get(&t.id) {
            if !description {
                now.description = old.description.clone();
            }
            if !tags {
                now.tags = old.tags.clone();
            }
        }
        seen.insert(t.id, now);
    }

    fn get(&self, id: i64) -> Option<Task> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned()
    }
}

/// A task, as `api#12` or just `12`. Agents are told it is a string, which keeps the
/// tool schemas short; a bare number is accepted too.
#[derive(Deserialize)]
#[serde(untagged)]
enum TaskKey {
    Number(i64),
    Text(String),
}

impl TaskKey {
    fn find(&self, store: &Store) -> store::Result<Task> {
        match self {
            TaskKey::Number(n) => store.get_task(*n),
            TaskKey::Text(s) => store.find_task(s),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct CreateProjectArgs {
    /// Short lowercase id, like api.
    slug: String,
    name: Option<String>,
    /// GitHub owner/repo.
    repo: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ListTasksArgs {
    /// Slug, or "all".
    project: Option<String>,
    /// One column, or "all". Done is left out by default.
    status: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct TaskArgs {
    /// Like api#12.
    #[schemars(with = "String")]
    id: TaskKey,
}

#[derive(Deserialize, JsonSchema)]
struct CreateTaskArgs {
    title: String,
    /// Markdown.
    description: Option<String>,
    /// Slug.
    project: Option<String>,
    /// Comma-separated.
    tags: Option<String>,
    /// Column; defaults to todo.
    status: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct UpdateTaskArgs {
    /// Like api#12.
    #[schemars(with = "String")]
    id: TaskKey,
    title: Option<String>,
    /// Markdown.
    description: Option<String>,
    /// Column, like review.
    status: Option<String>,
    /// Comma-separated; "" clears.
    tags: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct AddCommentArgs {
    /// Like api#12.
    #[schemars(with = "String")]
    id: TaskKey,
    /// Markdown.
    body: String,
    /// Defaults to your client's name.
    author: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct LinkRepoArgs {
    /// Slug.
    project: String,
    /// GitHub owner/repo.
    repo: String,
}

#[derive(Deserialize, JsonSchema)]
struct NextTaskArgs {
    /// Slug, or "all".
    project: Option<String>,
}

impl Kanzo {
    /// The tool list without the `$schema` line rmcp puts in every input schema, nor the
    /// `null` of optional fields: agents read this list in every session, so each byte counts.
    fn tools() -> ToolRouter<Self> {
        let mut router = Self::tool_router();
        for route in router.map.values_mut() {
            let mut schema = (*route.attr.input_schema).clone();
            schema.remove("$schema");
            // Optional fields are typed `["string", "null"]`; leaving them out of
            // `required` already says they're optional.
            if let Some(serde_json::Value::Object(props)) = schema.get_mut("properties") {
                for prop in props.values_mut() {
                    if let Some(kind) = prop.get_mut("type")
                        && let Some(first) = kind.as_array().and_then(|k| k.first()).cloned()
                    {
                        *kind = first;
                    }
                }
            }
            route.attr.input_schema = Arc::new(schema);
        }
        router
    }
}

#[tool_router]
impl Kanzo {
    /// Runs `f` with a fresh connection on a blocking thread and turns the
    /// outcome into text; errors go back to the agent as tool errors it can read.
    async fn run(&self, f: impl FnOnce(&Ctx, &Store) -> store::Result<String> + Send + 'static) -> CallToolResult {
        let ctx = self.ctx.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let store = ctx.home.store().map_err(|e| e.to_string())?;
            f(&ctx, &store).map_err(|e| e.to_string())
        })
        .await
        .unwrap_or_else(|e| Err(format!("internal error: {e}")));
        match outcome {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(text) => CallToolResult::error(vec![ContentBlock::text(text)]),
        }
    }

    #[tool(description = "List projects with their repos and open task counts.")]
    async fn list_projects(&self) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(|ctx, store| {
                let projects = store.list_projects()?;
                if projects.is_empty() {
                    return Ok("No projects yet. Create one with create_project.".into());
                }
                let here = projects.iter().find(|p| p.repos.iter().any(|r| ctx.repos.contains(r)));
                let mut out = String::new();
                for p in &projects {
                    out.push_str(&p.slug);
                    if p.name != p.slug {
                        let _ = write!(out, " \"{}\"", p.name);
                    }
                    if !p.repos.is_empty() {
                        let _ = write!(out, " ({})", p.repos.join(", "));
                    }
                    let _ = write!(out, ": {} open of {}", p.open, p.total);
                    if here.is_some_and(|h| h.id == p.id) {
                        out.push_str(" <- current repo");
                    }
                    out.push('\n');
                }
                out.pop();
                Ok(out)
            })
            .await)
    }

    #[tool(description = "Create a project.")]
    async fn create_project(
        &self,
        Parameters(args): Parameters<CreateProjectArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |_, store| {
                let p = store.create_project(&args.slug, args.name.as_deref().unwrap_or(""), args.repo.as_deref())?;
                let repo = p.repos.first().map(|r| format!(", linked to {r}")).unwrap_or_default();
                Ok(format!("Created project {} ({}){repo}.", p.slug, p.name))
            })
            .await)
    }

    #[tool(description = "Link another GitHub repo to a project.")]
    async fn link_repo(&self, Parameters(args): Parameters<LinkRepoArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |_, store| {
                let p = store.link_repo(&args.project, &args.repo)?;
                Ok(format!("Project {} is linked to {}.", p.slug, p.repos.join(", ")))
            })
            .await)
    }

    #[tool(description = "List tasks in board order.")]
    async fn list_tasks(&self, Parameters(args): Parameters<ListTasksArgs>) -> Result<CallToolResult, ErrorData> {
        let seen = self.seen.clone();
        Ok(self
            .run(move |ctx, store| {
                let all = args.project.as_deref().is_some_and(|p| p.trim().eq_ignore_ascii_case("all"));
                let project = if all { None } else { Some(ctx.project(store, args.project.as_deref())?) };
                let status = args.status.as_deref().map(str::trim).filter(|s| !s.is_empty());
                let only = match status {
                    Some("all") | None => None,
                    Some(s) if store.statuses().iter().any(|x| x == s) => Some(s),
                    Some(s) => {
                        return Err(Error::Invalid(format!(
                            "unknown status {s:?}; use one of: {}, or all",
                            store.statuses().join(", ")
                        )));
                    }
                };
                // Done tasks only pile up: without a status leave them out, with one keep
                // the newest.
                let keep = if status.is_none() { 0 } else { DONE_LISTED };
                let (tasks, hidden) =
                    store.list_tasks_keeping_done(project.as_ref().map(|p| p.id), only, Some(keep))?;
                let done = store.statuses().last().map_or("", String::as_str);
                let mut out = String::new();
                // Tasks for a person are left out: agents leave them alone.
                let mut people = 0;
                for t in &tasks {
                    if t.human {
                        people += 1;
                    } else {
                        seen.saw_some(t, false, true);
                        task_line(&mut out, t);
                        out.push('\n');
                    }
                }
                if out.is_empty() {
                    let scope = project.as_ref().map_or("any project", |p| p.slug.as_str());
                    let _ = writeln!(out, "No tasks in {scope}.");
                }
                if people > 0 {
                    let tasks = if people == 1 { "task" } else { "tasks" };
                    let _ = writeln!(out, "({people} human {tasks} not shown: for the user, not for agents)");
                }
                if hidden > 0 && status.is_none() {
                    let _ = writeln!(out, "({hidden} {done} not shown; pass status \"{done}\" or \"all\")");
                } else if hidden > 0 {
                    let _ = writeln!(out, "({hidden} older {done} not shown)");
                }
                out.pop();
                Ok(out)
            })
            .await)
    }

    #[tool(description = "Show a task with its comments and image paths.")]
    async fn get_task(&self, Parameters(args): Parameters<TaskArgs>) -> Result<CallToolResult, ErrorData> {
        let seen = self.seen.clone();
        Ok(self
            .run(move |ctx, store| {
                let task = args.id.find(store)?;
                seen.saw(&task);
                task_detail(ctx, store, &task)
            })
            .await)
    }

    #[tool(description = "Add a task, to todo unless you pass status.")]
    async fn create_task(&self, Parameters(args): Parameters<CreateTaskArgs>) -> Result<CallToolResult, ErrorData> {
        let seen = self.seen.clone();
        Ok(self
            .run(move |ctx, store| {
                let project = ctx.project(store, args.project.as_deref())?;
                let status = args.status.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
                let description = args.description.as_deref().unwrap_or("");
                let mut t = store.create_task_in(project.id, status.as_deref(), &args.title, description)?;
                if args.tags.is_some() {
                    t = store.update_task(t.id, TaskPatch { tags: args.tags, ..Default::default() }, None)?.into_task();
                }
                seen.saw(&t);
                Ok(format!("Created {} in {}: {}", t.key(), t.status, t.title))
            })
            .await)
    }

    #[tool(description = "Change the fields you pass; an edit over a change you haven't read goes to a copy.")]
    async fn update_task(&self, Parameters(args): Parameters<UpdateTaskArgs>) -> Result<CallToolResult, ErrorData> {
        let seen = self.seen.clone();
        Ok(self
            .run(move |_, store| {
                let before = args.id.find(store)?;
                let status = args.status.map(|s| s.trim().to_ascii_lowercase());
                let wrote = (args.description.is_some(), args.tags.is_some());
                let patch = TaskPatch {
                    title: args.title,
                    description: args.description,
                    status,
                    tags: args.tags,
                    project_id: None,
                    human: None,
                };
                let after = match store.update_task(before.id, patch, seen.get(before.id).as_ref())? {
                    Updated::Applied(after) => after,
                    Updated::Copied { original, copy, fields } => {
                        seen.saw(&copy);
                        let (key, copy_key) = (original.key(), copy.key());
                        return Ok(format!(
                            "Conflict: {} of {key} changed since you last read it. Your edit was saved as a new task \
                             {copy_key} (tagged conflict); {key} is unchanged. Compare them and merge by hand.",
                            fields.join(", ")
                        ));
                    }
                };
                // The reply shows the title and status, not the rest.
                seen.saw_some(&after, wrote.0, wrote.1);
                let moved = if before.status != after.status {
                    format!(", {} -> {}", before.status, after.status)
                } else {
                    String::new()
                };
                Ok(format!("Updated {}{moved}: {}", after.key(), after.title))
            })
            .await)
    }

    #[tool(description = "Comment on a task, like to report progress.")]
    async fn add_comment(
        &self,
        Parameters(args): Parameters<AddCommentArgs>,
        peer: Peer<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = peer.peer_info().map(|i| i.client_info.name.clone()).filter(|n| !n.is_empty());
        let author = args.author.filter(|a| !a.trim().is_empty()).or(client).unwrap_or_else(|| "agent".into());
        Ok(self
            .run(move |_, store| {
                let task = args.id.find(store)?;
                store.add_comment(task.id, &author, &args.body, &[])?;
                Ok(format!("Comment added to {} as {author}.", task.key()))
            })
            .await)
    }

    #[tool(description = "Take the top todo task, move it to doing and show it.")]
    async fn next_task(&self, Parameters(args): Parameters<NextTaskArgs>) -> Result<CallToolResult, ErrorData> {
        let seen = self.seen.clone();
        Ok(self
            .run(move |ctx, store| {
                let all = args.project.as_deref().is_some_and(|p| p.trim().eq_ignore_ascii_case("all"));
                let project = if all { None } else { Some(ctx.project(store, args.project.as_deref())?) };
                match store.next_task(project.as_ref().map(|p| p.id))? {
                    Some(task) => {
                        seen.saw(&task);
                        Ok(format!("Moved to {}:\n{}", task.status, task_detail(ctx, store, &task)?))
                    }
                    None => {
                        let scope = project.map_or("any project".into(), |p| p.slug);
                        Ok(format!("Nothing in todo for {scope}."))
                    }
                }
            })
            .await)
    }

    #[tool(description = "Delete a task with its comments and images. Can't be undone.")]
    async fn delete_task(&self, Parameters(args): Parameters<TaskArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |ctx, store| {
                let task = args.id.find(store)?;
                ctx.home.deleting_images(store, store::ImagesOf::Task(task.id), || store.delete_task(task.id))?;
                let comments = match task.comments {
                    0 => String::new(),
                    1 => " and its comment".into(),
                    n => format!(" and its {n} comments"),
                };
                Ok(format!("Deleted {}{comments}: {}", task.key(), task.title))
            })
            .await)
    }
}

// Serve the router built once in `tools()`; by default the macro builds a new,
// untrimmed one for every request.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Kanzo {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("kanzo", env!("CARGO_PKG_VERSION")))
            .with_instructions(if self.ctx.cwd.is_some() { INSTRUCTIONS } else { HTTP_INSTRUCTIONS })
    }
}

/// Most tasks `prime` lists, so a big board doesn't flood the session.
const PRIME_LISTED: usize = 40;

/// How long `prime` waits for its input, and for sync; hooks give it a few seconds.
const PRIME_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// `kanzo prime`: for an agent's SessionStart hook, the open tasks of the project linked
/// to the session's folder, as hook JSON. Prints nothing for an unlinked folder or on any
/// error, so a hook never breaks a session.
pub fn prime(home: Home) {
    use std::io::{IsTerminal, Read};
    let input = if std::io::stdin().is_terminal() {
        String::new()
    } else {
        // A runner that never closes stdin would otherwise hold us until its timeout.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = std::io::stdin().read_to_string(&mut s);
            let _ = tx.send(s);
        });
        rx.recv_timeout(PRIME_WAIT).unwrap_or_default()
    };
    if let Some(out) = prime_json(home, &input) {
        println!("{out}");
    }
}

/// The hook output for `input`, the hook's JSON, which names the session's folder;
/// without it, ours.
fn prime_json(home: Home, input: &str) -> Option<String> {
    let cwd = serde_json::from_str::<serde_json::Value>(input)
        .ok()
        .and_then(|v| v["cwd"].as_str().map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok())?;
    let repos = git_repos(&cwd);
    let ctx = Ctx { home, cwd: Some(cwd), repos };
    let text = prime_text(&ctx).ok()?;
    let out =
        serde_json::json!({ "hookSpecificOutput": { "hookEventName": "SessionStart", "additionalContext": text } });
    Some(out.to_string())
}

/// Fails, like the tools, when the folder is linked to no project.
fn prime_text(ctx: &Ctx) -> store::Result<String> {
    let store = ctx.home.store_waiting(PRIME_WAIT)?;
    let project = ctx.project(&store, None)?;
    let (tasks, _) = store.list_tasks_keeping_done(Some(project.id), None, Some(0))?;
    let (people, agents): (Vec<&Task>, Vec<&Task>) = tasks.iter().partition(|t| t.human);
    let slug = &project.slug;
    let mut out = format!("Kanzo board: this repo is project `{slug}`. ");
    if agents.is_empty() {
        out.push_str("It has no open tasks for agents.");
    } else {
        out.push_str("Open tasks, keep them current with the kanzo tools:\n");
        for t in agents.iter().take(PRIME_LISTED) {
            task_line(&mut out, t);
            out.push('\n');
        }
        if agents.len() > PRIME_LISTED {
            let _ = writeln!(out, "({} more: list_tasks)", agents.len() - PRIME_LISTED);
        }
        out.pop();
    }
    if !people.is_empty() {
        let n = people.len();
        let tasks = if n == 1 { "task" } else { "tasks" };
        let _ = write!(out, "\n({n} human {tasks} not shown: for the user, not for agents)");
    }
    Ok(out)
}

/// `api#12 [todo] Fix login {bug} (2 comments)`.
fn task_line(out: &mut String, t: &Task) {
    let _ = write!(out, "{}#{} [{}] {}", t.project, t.id, t.status, t.title);
    write_tags(out, t);
    match t.comments {
        0 => {}
        1 => out.push_str(" (1 comment)"),
        n => {
            let _ = write!(out, " ({n} comments)");
        }
    }
}

/// ` {bug, ui}`, or nothing without tags.
fn write_tags(out: &mut String, t: &Task) {
    if !t.tags.is_empty() {
        let _ = write!(out, " {{{}}}", t.tags.join(", "));
    }
}

/// A task in full: description, comments, and the paths of its images, which agents can open.
fn task_detail(ctx: &Ctx, store: &Store, t: &Task) -> store::Result<String> {
    let comments = store.list_comments(t.id)?;
    let images = store.list_images(t.id)?;
    let dir = ctx.home.dir.join("images");
    let paths = |comment: Option<i64>| -> Vec<String> {
        images.iter().filter(|i| i.comment_id == comment).map(|i| dir.join(&i.file).display().to_string()).collect()
    };
    let (created, updated) = (short_time(&t.created_at), short_time(&t.updated_at));
    let mut out = format!("{}#{} {}", t.project, t.id, t.title);
    write_tags(&mut out, t);
    let _ = write!(out, "\nstatus: {}", t.status);
    if t.human {
        out.push_str(", human: for the user, not for agents");
    }
    let _ = write!(out, ", created {created}");
    // The update time only when it says something the creation time does not.
    if updated != created {
        let _ = write!(out, ", updated {updated}");
    }
    out.push('\n');
    if !t.description.is_empty() {
        let _ = writeln!(out, "\n{}", t.description);
    }
    let own = paths(None);
    if !own.is_empty() {
        let _ = writeln!(out, "\nImages: {}", own.join(", "));
    }
    if !comments.is_empty() {
        let _ = writeln!(out, "\nComments ({}):", comments.len());
        for c in &comments {
            let _ = write!(out, "- {}, {}: {}", c.author, short_time(&c.created_at), c.body);
            let attached = paths(Some(c.id));
            if !attached.is_empty() {
                let sep = if c.body.is_empty() { "" } else { " " };
                let _ = write!(out, "{sep}[images: {}]", attached.join(", "));
            }
            out.push('\n');
        }
    }
    out.truncate(out.trim_end().len());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prime_lists_the_linked_projects_open_tasks() {
        let home_dir = tempfile::tempdir().unwrap();
        assert!(Home::existing(home_dir.path().to_path_buf()).is_none(), "no board yet");
        assert!(!home_dir.path().join("config.toml").exists(), "and nothing made");
        let home = Home::open(home_dir.path().to_path_buf()).unwrap();
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(Command::new("git").arg("-C").arg(repo.path()).args(args).status().unwrap().success())
        };
        // As a hook sends it: JSON, with the session's folder escaped.
        let input = serde_json::json!({ "cwd": repo.path(), "hook_event_name": "SessionStart" }).to_string();
        let prime = || prime_json(Home::existing(home.dir.clone()).unwrap(), &input);
        git(&["init", "--quiet"]);
        assert_eq!(prime(), None, "no remote, no output");
        git(&["remote", "add", "origin", "https://github.com/you/api.git"]);
        assert_eq!(prime(), None, "remote linked to no project, no output");
        let s = home.store().unwrap();
        let p = s.create_project("api", "API", Some("you/api")).unwrap();
        let open = s.create_task(p.id, "open one", "").unwrap();
        let done = s.create_task(p.id, "finished", "").unwrap();
        let mine = s.create_task(p.id, "for a person", "").unwrap();
        let patch =
            |status: Option<&str>, human| TaskPatch { status: status.map(String::from), human, ..Default::default() };
        s.update_task(done.id, patch(Some("done"), None), None).unwrap();
        s.update_task(mine.id, patch(None, Some(true)), None).unwrap();
        let out: serde_json::Value = serde_json::from_str(&prime().unwrap()).unwrap();
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "SessionStart");
        let text = out["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
        assert!(text.contains(&format!("api#{} [todo] open one", open.id)), "{text}");
        assert!(!text.contains("finished") && !text.contains("for a person"), "{text}");
        assert!(text.contains("1 human task not shown"), "{text}");
    }
}
