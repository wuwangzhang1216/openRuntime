use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::Mutex,
};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    children: Arc<Mutex<HashMap<Uuid, RunningProcess>>>,
}

struct RunningProcess {
    child: Child,
    stdin: Option<ChildStdin>,
}

#[derive(Debug, Clone, Serialize)]
struct Task {
    id: Uuid,
    title: String,
    prompt: String,
    runner: RunnerKind,
    command: String,
    workspace: String,
    worktree_path: Option<String>,
    base_commit: Option<String>,
    diff_stat: Option<String>,
    approved_at: Option<DateTime<Utc>>,
    worktree_merged_at: Option<DateTime<Utc>>,
    worktree_cleaned_at: Option<DateTime<Utc>>,
    status: TaskStatus,
    budget_minutes: u32,
    policy: TaskPolicy,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    events: Vec<TaskEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RunnerKind {
    Shell,
    ClaudeCode,
    Codex,
}

impl RunnerKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }
}

impl FromStr for RunnerKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "shell" => Ok(Self::Shell),
            "claude-code" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            _ => Err(format!("unknown runner kind: {value}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum TaskStatus {
    Queued,
    Running,
    NeedsInput,
    ReadyForReview,
    Completed,
    Failed,
    Stopped,
}

impl TaskStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::NeedsInput => "needs-input",
            Self::ReadyForReview => "ready-for-review",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

impl FromStr for TaskStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "needs-input" => Ok(Self::NeedsInput),
            "ready-for-review" => Ok(Self::ReadyForReview),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "stopped" => Ok(Self::Stopped),
            _ => Err(format!("unknown task status: {value}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskPolicy {
    allow_network: bool,
    allow_git_write: bool,
    allow_secrets: bool,
    require_approval: bool,
    blocked_commands: Vec<String>,
}

impl Default for TaskPolicy {
    fn default() -> Self {
        Self {
            allow_network: false,
            allow_git_write: false,
            allow_secrets: false,
            require_approval: false,
            blocked_commands: vec![
                "rm -rf".to_string(),
                "sudo".to_string(),
                "git push".to_string(),
                "curl".to_string(),
                "wget".to_string(),
                "ssh".to_string(),
                "scp".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct TaskEvent {
    id: Uuid,
    task_id: Uuid,
    kind: EventKind,
    message: String,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
enum EventKind {
    Lifecycle,
    Stdout,
    Stderr,
    Diff,
    Input,
    Error,
}

impl EventKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Lifecycle => "lifecycle",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Diff => "diff",
            Self::Input => "input",
            Self::Error => "error",
        }
    }
}

impl FromStr for EventKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "lifecycle" => Ok(Self::Lifecycle),
            "stdout" => Ok(Self::Stdout),
            "stderr" => Ok(Self::Stderr),
            "diff" => Ok(Self::Diff),
            "input" => Ok(Self::Input),
            "error" => Ok(Self::Error),
            _ => Err(format!("unknown event kind: {value}")),
        }
    }
}

#[derive(Debug, Deserialize)]
struct CreateTaskRequest {
    title: String,
    prompt: String,
    runner: RunnerKind,
    command: Option<String>,
    workspace: Option<String>,
    budget_minutes: Option<u32>,
    policy: Option<TaskPolicy>,
}

#[derive(Debug, Deserialize)]
struct ApproveTaskRequest {
    note: Option<String>,
    start: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ReplyTaskRequest {
    message: String,
}

#[derive(Debug, Deserialize)]
struct RegisterWorkspaceRequest {
    path: String,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    storage: &'static str,
}

#[derive(Debug, Serialize)]
struct RunnerInfo {
    runner: RunnerKind,
    available: bool,
    command: String,
}

#[derive(Debug, Serialize)]
struct WorkspaceProject {
    name: String,
    path: String,
}

#[derive(Debug, Serialize)]
struct TaskDiffResponse {
    task_id: Uuid,
    isolated: bool,
    worktree_path: Option<String>,
    base_commit: Option<String>,
    stat: String,
    patch: String,
}

#[derive(Debug, Serialize)]
struct WorktreeActionResponse {
    task: Task,
    message: String,
}

type ApiResult<T> = Result<Json<T>, (StatusCode, String)>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("openruntime_backend=debug,tower_http=debug")
        .init();

    let db = open_database().await.expect("open sqlite database");
    init_database(&db)
        .await
        .expect("initialize sqlite database");
    mark_orphaned_running_tasks(&db)
        .await
        .expect("recover task state");

    let state = AppState {
        db,
        children: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/runners", get(list_runners))
        .route("/workspaces", get(list_workspaces))
        .route("/workspaces/pick", post(pick_workspace))
        .route("/workspaces/register", post(register_workspace))
        .route("/tasks", get(list_tasks).post(create_task))
        .route("/tasks/{id}", get(get_task))
        .route("/tasks/{id}/diff", get(get_task_diff))
        .route("/tasks/{id}/events", get(list_events))
        .route("/tasks/{id}/approve", post(approve_task))
        .route("/tasks/{id}/reply", post(reply_task))
        .route("/tasks/{id}/start", post(start_task))
        .route("/tasks/{id}/stop", post(stop_task))
        .route("/tasks/{id}/worktree/merge", post(merge_task_worktree))
        .route("/tasks/{id}/worktree/cleanup", post(cleanup_task_worktree))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let port = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind backend listener");

    println!("openRuntime backend listening on http://{addr}");
    axum::serve(listener, app).await.expect("serve backend");
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        storage: "sqlite",
    })
}

async fn list_runners() -> Json<Vec<RunnerInfo>> {
    Json(vec![
        RunnerInfo {
            runner: RunnerKind::Codex,
            available: find_executable("codex").is_some(),
            command: "codex exec --json --skip-git-repo-check -s workspace-write".to_string(),
        },
        RunnerInfo {
            runner: RunnerKind::ClaudeCode,
            available: find_executable("claude").is_some(),
            command: "claude -p".to_string(),
        },
        RunnerInfo {
            runner: RunnerKind::Shell,
            available: true,
            command: "/bin/sh -lc".to_string(),
        },
    ])
}

async fn list_workspaces(State(state): State<AppState>) -> ApiResult<Vec<WorkspaceProject>> {
    let mut projects = Vec::new();
    let current_dir = std::env::current_dir().map_err(internal_error)?;
    push_workspace_project(&mut projects, current_dir);

    let rows = sqlx::query(
        r#"
        SELECT workspace, MAX(updated_at) AS last_seen
        FROM tasks
        GROUP BY workspace
        ORDER BY last_seen DESC
        "#,
    )
    .fetch_all(&state.db)
    .await
    .map_err(internal_error)?;

    for row in rows {
        push_workspace_project(
            &mut projects,
            PathBuf::from(row.get::<String, _>("workspace")),
        );
    }

    let rows = sqlx::query(
        r#"
        SELECT path
        FROM workspaces
        ORDER BY updated_at DESC
        "#,
    )
    .fetch_all(&state.db)
    .await
    .map_err(internal_error)?;

    for row in rows {
        push_workspace_project(&mut projects, PathBuf::from(row.get::<String, _>("path")));
    }

    projects.dedup_by(|left, right| left.path == right.path);

    Ok(Json(projects))
}

async fn register_workspace(
    State(state): State<AppState>,
    Json(payload): Json<RegisterWorkspaceRequest>,
) -> ApiResult<WorkspaceProject> {
    let path = resolve_workspace(Some(payload.path))?;
    let project = workspace_project_from_path(PathBuf::from(&path)).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "workspace has no folder name".to_string(),
        )
    })?;
    let now = Utc::now().to_rfc3339();

    sqlx::query(
        r#"
        INSERT INTO workspaces (path, name, created_at, updated_at)
        VALUES (?, ?, ?, ?)
        ON CONFLICT(path) DO UPDATE SET name = excluded.name, updated_at = excluded.updated_at
        "#,
    )
    .bind(&project.path)
    .bind(&project.name)
    .bind(&now)
    .bind(&now)
    .execute(&state.db)
    .await
    .map_err(internal_error)?;

    Ok(Json(project))
}

async fn pick_workspace() -> ApiResult<Option<WorkspaceProject>> {
    let Some(path) = pick_folder_path().await? else {
        return Ok(Json(None));
    };

    let path = path.canonicalize().map_err(internal_error)?;
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return Err((
            StatusCode::BAD_REQUEST,
            "selected workspace has no folder name".to_string(),
        ));
    };

    Ok(Json(Some(WorkspaceProject {
        name: name.to_string(),
        path: path.to_string_lossy().to_string(),
    })))
}

#[cfg(target_os = "macos")]
async fn pick_folder_path() -> Result<Option<PathBuf>, (StatusCode, String)> {
    let output = Command::new("osascript")
        .arg("-e")
        .arg(r#"POSIX path of (choose folder with prompt "Choose agent workspace")"#)
        .output()
        .await
        .map_err(internal_error)?;

    if !output.status.success() {
        return Ok(None);
    }

    let path = String::from_utf8(output.stdout)
        .map_err(internal_error)?
        .trim()
        .to_string();

    if path.is_empty() {
        return Ok(None);
    }

    Ok(Some(PathBuf::from(path)))
}

#[cfg(not(target_os = "macos"))]
async fn pick_folder_path() -> Result<Option<PathBuf>, (StatusCode, String)> {
    Err((
        StatusCode::NOT_IMPLEMENTED,
        "Native folder picker is only implemented on macOS right now".to_string(),
    ))
}

async fn list_tasks(State(state): State<AppState>) -> ApiResult<Vec<Task>> {
    let ids = sqlx::query("SELECT id FROM tasks ORDER BY updated_at DESC")
        .fetch_all(&state.db)
        .await
        .map_err(internal_error)?
        .into_iter()
        .map(|row| row.get::<String, _>("id"))
        .collect::<Vec<_>>();

    let mut tasks = Vec::with_capacity(ids.len());
    for id in ids {
        let id = parse_uuid(&id)?;
        if let Some(task) = load_task(&state.db, id).await.map_err(internal_error)? {
            tasks.push(task);
        }
    }

    Ok(Json(tasks))
}

async fn create_task(
    State(state): State<AppState>,
    Json(payload): Json<CreateTaskRequest>,
) -> ApiResult<Task> {
    if payload.title.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "title is required".to_string()));
    }

    let workspace = resolve_workspace(payload.workspace)?;
    let prompt = payload.prompt.trim().to_string();
    let command = normalize_command(&payload.runner, payload.command, &prompt)?;
    let id = Uuid::new_v4();
    let now = Utc::now();
    let policy = payload.policy.unwrap_or_default();
    let policy_json = serde_json::to_string(&policy).map_err(internal_error)?;

    sqlx::query(
        r#"
        INSERT INTO tasks (
            id, title, prompt, runner, command, workspace, status,
            budget_minutes, policy_json, created_at, updated_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(id.to_string())
    .bind(payload.title.trim())
    .bind(&prompt)
    .bind(payload.runner.as_str())
    .bind(command)
    .bind(workspace)
    .bind(TaskStatus::Queued.as_str())
    .bind(i64::from(
        payload.budget_minutes.unwrap_or(15).clamp(1, 240),
    ))
    .bind(policy_json)
    .bind(now.to_rfc3339())
    .bind(now.to_rfc3339())
    .execute(&state.db)
    .await
    .map_err(internal_error)?;

    insert_event(
        &state.db,
        id,
        EventKind::Lifecycle,
        "Task created".to_string(),
    )
    .await
    .map_err(internal_error)?;

    load_task(&state.db, id)
        .await
        .map_err(internal_error)?
        .map(Json)
        .ok_or_else(|| internal_error("task disappeared after create"))
}

async fn get_task(State(state): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Task> {
    load_task(&state.db, id)
        .await
        .map_err(internal_error)?
        .map(Json)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "task not found".to_string()))
}

async fn list_events(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Vec<TaskEvent>> {
    if load_task_row(&state.db, id)
        .await
        .map_err(internal_error)?
        .is_none()
    {
        return Err((StatusCode::NOT_FOUND, "task not found".to_string()));
    }

    Ok(Json(
        load_events(&state.db, id).await.map_err(internal_error)?,
    ))
}

async fn get_task_diff(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<TaskDiffResponse> {
    let task = load_task(&state.db, id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "task not found".to_string()))?;

    let Some(worktree_path) = task.worktree_path.clone() else {
        return Ok(Json(TaskDiffResponse {
            task_id: id,
            isolated: false,
            worktree_path: None,
            base_commit: None,
            stat: task.diff_stat.unwrap_or_default(),
            patch: String::new(),
        }));
    };

    let stat = git_output_lossy(&worktree_path, &["diff", "--stat", "HEAD", "--"])
        .await
        .unwrap_or_default();
    let patch = git_output_lossy(&worktree_path, &["diff", "HEAD", "--"])
        .await
        .unwrap_or_else(|error| format!("Could not read diff: {error}"));
    let untracked = untracked_patch(&worktree_path).await.unwrap_or_default();
    let patch = if untracked.trim().is_empty() {
        patch
    } else {
        format!("{patch}\n\n{untracked}")
    };

    Ok(Json(TaskDiffResponse {
        task_id: id,
        isolated: true,
        worktree_path: Some(worktree_path),
        base_commit: task.base_commit,
        stat: if stat.trim().is_empty() {
            task.diff_stat.unwrap_or_default()
        } else {
            stat
        },
        patch,
    }))
}

async fn approve_task(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<ApproveTaskRequest>,
) -> ApiResult<Task> {
    let task = load_task(&state.db, id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "task not found".to_string()))?;

    let now = Utc::now();
    sqlx::query("UPDATE tasks SET approved_at = ?, updated_at = ? WHERE id = ?")
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(id.to_string())
        .execute(&state.db)
        .await
        .map_err(internal_error)?;

    let note = payload
        .note
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!(": {value}"))
        .unwrap_or_default();
    insert_event(
        &state.db,
        id,
        EventKind::Input,
        format!("Task approved{note}"),
    )
    .await
    .map_err(internal_error)?;

    if payload.start.unwrap_or(true) && task.status != TaskStatus::Running {
        return start_task(State(state), Path(id)).await;
    }

    get_task(State(state), Path(id)).await
}

async fn reply_task(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<ReplyTaskRequest>,
) -> ApiResult<Task> {
    let message = payload.message.trim().to_string();
    if message.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "reply is required".to_string()));
    }

    if load_task_row(&state.db, id)
        .await
        .map_err(internal_error)?
        .is_none()
    {
        return Err((StatusCode::NOT_FOUND, "task not found".to_string()));
    }

    insert_event(
        &state.db,
        id,
        EventKind::Input,
        format!("User reply: {message}"),
    )
    .await
    .map_err(internal_error)?;

    let mut children = state.children.lock().await;
    if let Some(process) = children.get_mut(&id) {
        if let Some(stdin) = process.stdin.as_mut() {
            stdin
                .write_all(format!("{message}\n").as_bytes())
                .await
                .map_err(|error| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("failed to send reply: {error}"),
                    )
                })?;
            stdin.flush().await.map_err(|error| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to flush reply: {error}"),
                )
            })?;
        }
    }
    drop(children);

    get_task(State(state), Path(id)).await
}

async fn start_task(State(state): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Task> {
    let mut task = load_task(&state.db, id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "task not found".to_string()))?;

    if task.status == TaskStatus::Running {
        return Err((StatusCode::CONFLICT, "task is already running".to_string()));
    }

    if let Err(message) = validate_policy(&task) {
        update_status(
            &state.db,
            id,
            TaskStatus::NeedsInput,
            EventKind::Error,
            message.clone(),
        )
        .await
        .map_err(internal_error)?;
        return Err((StatusCode::FORBIDDEN, message));
    }

    let execution_workspace = prepare_execution_workspace(&state.db, &task)
        .await
        .map_err(internal_error)?;
    task = load_task(&state.db, id)
        .await
        .map_err(internal_error)?
        .unwrap_or(task);

    let (runner_label, mut command) =
        build_runner_command(&task, &execution_workspace).map_err(|message| {
            (
                StatusCode::PRECONDITION_FAILED,
                format!("Cannot start {} runner: {message}", task.runner.as_str()),
            )
        })?;

    update_status(
        &state.db,
        id,
        TaskStatus::Running,
        EventKind::Lifecycle,
        format!("Starting {runner_label} runner in {execution_workspace}"),
    )
    .await
    .map_err(internal_error)?;

    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let keep_stdin = matches!(task.runner, RunnerKind::Shell);
    command.stdin(if keep_stdin {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    });

    let mut child = command.spawn().map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to start command: {error}"),
        )
    })?;

    if let Some(stdout) = child.stdout.take() {
        spawn_output_reader(state.clone(), id, stdout, EventKind::Stdout);
    }

    if let Some(stderr) = child.stderr.take() {
        spawn_output_reader(state.clone(), id, stderr, EventKind::Stderr);
    }

    let stdin = keep_stdin.then(|| child.stdin.take()).flatten();
    state
        .children
        .lock()
        .await
        .insert(id, RunningProcess { child, stdin });
    spawn_task_monitor(state.clone(), id, task.budget_minutes);

    get_task(State(state), Path(id)).await
}

async fn stop_task(State(state): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Task> {
    let mut process = state.children.lock().await.remove(&id);

    if let Some(process) = process.as_mut() {
        process.child.kill().await.map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to stop command: {error}"),
            )
        })?;
    }

    capture_task_diff(&state.db, id)
        .await
        .map_err(internal_error)?;

    update_status(
        &state.db,
        id,
        TaskStatus::Stopped,
        EventKind::Lifecycle,
        "Task stopped by user".to_string(),
    )
    .await
    .map_err(internal_error)?;

    get_task(State(state), Path(id)).await
}

async fn merge_task_worktree(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<WorktreeActionResponse> {
    let task = load_task(&state.db, id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "task not found".to_string()))?;
    let Some(worktree_path) = task.worktree_path.clone() else {
        return Err((StatusCode::BAD_REQUEST, "task has no worktree".to_string()));
    };

    if task.worktree_merged_at.is_some() {
        return Err((
            StatusCode::CONFLICT,
            "worktree has already been merged".to_string(),
        ));
    }

    capture_task_diff(&state.db, id)
        .await
        .map_err(internal_error)?;
    let source_status = git_output_lossy(&worktree_path, &["status", "--short"])
        .await
        .map_err(internal_error)?;
    if source_status.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "worktree has no changes to merge".to_string(),
        ));
    }

    let repo_root = git_output_lossy(&task.workspace, &["rev-parse", "--show-toplevel"])
        .await
        .map_err(internal_error)?
        .trim()
        .to_string();
    let source_root = PathBuf::from(&worktree_path);

    if let Err(error) = git_output_lossy(
        &repo_root,
        &["diff", "--quiet", "--ignore-submodules", "--"],
    )
    .await
    {
        return Err((
            StatusCode::CONFLICT,
            format!("target workspace has uncommitted tracked changes: {error}"),
        ));
    }

    let patch = git_output_lossy(&worktree_path, &["diff", "--binary", "HEAD", "--"])
        .await
        .map_err(internal_error)?;
    apply_patch_to_target(&repo_root, &patch)
        .await
        .map_err(internal_error)?;

    let untracked = git_output_lossy(
        &worktree_path,
        &["ls-files", "--others", "--exclude-standard"],
    )
    .await
    .unwrap_or_default();
    for file in untracked
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        copy_worktree_file(&source_root, FsPath::new(&repo_root), file)
            .await
            .map_err(internal_error)?;
    }

    let now = Utc::now();
    sqlx::query("UPDATE tasks SET worktree_merged_at = ?, updated_at = ? WHERE id = ?")
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(id.to_string())
        .execute(&state.db)
        .await
        .map_err(internal_error)?;
    insert_event(
        &state.db,
        id,
        EventKind::Lifecycle,
        "Merged worktree changes into target workspace".to_string(),
    )
    .await
    .map_err(internal_error)?;

    let task = load_task(&state.db, id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| internal_error("task disappeared after merge"))?;

    Ok(Json(WorktreeActionResponse {
        task,
        message: "Merged worktree changes into target workspace".to_string(),
    }))
}

async fn cleanup_task_worktree(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<WorktreeActionResponse> {
    let task = load_task(&state.db, id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "task not found".to_string()))?;
    let Some(worktree_path) = task.worktree_path.clone() else {
        return Err((StatusCode::BAD_REQUEST, "task has no worktree".to_string()));
    };

    if task.status == TaskStatus::Running {
        return Err((
            StatusCode::CONFLICT,
            "cannot clean up a running task".to_string(),
        ));
    }

    let repo_root = git_output_lossy(&task.workspace, &["rev-parse", "--show-toplevel"])
        .await
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|_| task.workspace.clone());
    let _ = git_output_lossy(
        &repo_root,
        &["worktree", "remove", "--force", &worktree_path],
    )
    .await;
    if FsPath::new(&worktree_path).exists() {
        tokio::fs::remove_dir_all(&worktree_path)
            .await
            .map_err(internal_error)?;
    }

    let now = Utc::now();
    sqlx::query("UPDATE tasks SET worktree_cleaned_at = ?, updated_at = ? WHERE id = ?")
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(id.to_string())
        .execute(&state.db)
        .await
        .map_err(internal_error)?;
    insert_event(
        &state.db,
        id,
        EventKind::Lifecycle,
        "Cleaned up isolated worktree".to_string(),
    )
    .await
    .map_err(internal_error)?;

    let task = load_task(&state.db, id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| internal_error("task disappeared after cleanup"))?;

    Ok(Json(WorktreeActionResponse {
        task,
        message: "Cleaned up isolated worktree".to_string(),
    }))
}

fn spawn_output_reader<R>(state: AppState, task_id: Uuid, reader: R, kind: EventKind)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Err(error) = insert_event(&state.db, task_id, kind.clone(), line).await {
                eprintln!("failed to persist task output: {error}");
            }
        }
    });
}

fn spawn_task_monitor(state: AppState, task_id: Uuid, budget_minutes: u32) {
    tokio::spawn(async move {
        let mut elapsed = Duration::ZERO;
        let tick = Duration::from_millis(500);
        let budget = Duration::from_secs(u64::from(budget_minutes) * 60);

        loop {
            tokio::time::sleep(tick).await;
            elapsed += tick;

            if elapsed >= budget {
                if let Some(mut process) = state.children.lock().await.remove(&task_id) {
                    let _ = process.child.kill().await;
                    let _ = capture_task_diff(&state.db, task_id).await;
                    let _ = update_status(
                        &state.db,
                        task_id,
                        TaskStatus::Stopped,
                        EventKind::Lifecycle,
                        format!("Task exceeded budget of {budget_minutes} minute(s)"),
                    )
                    .await;
                }
                return;
            }

            let status = {
                let mut children = state.children.lock().await;
                let Some(process) = children.get_mut(&task_id) else {
                    return;
                };

                match process.child.try_wait() {
                    Ok(Some(status)) => {
                        children.remove(&task_id);
                        Some(Ok(status))
                    }
                    Ok(None) => None,
                    Err(error) => Some(Err(error)),
                }
            };

            match status {
                Some(Ok(exit_status)) if exit_status.success() => {
                    let has_changes = capture_task_diff(&state.db, task_id).await.unwrap_or(false);
                    let _ = update_status(
                        &state.db,
                        task_id,
                        if has_changes {
                            TaskStatus::ReadyForReview
                        } else {
                            TaskStatus::Completed
                        },
                        EventKind::Lifecycle,
                        format!("Task completed with status {exit_status}"),
                    )
                    .await;
                    return;
                }
                Some(Ok(exit_status)) => {
                    let _ = capture_task_diff(&state.db, task_id).await;
                    let _ = update_status(
                        &state.db,
                        task_id,
                        TaskStatus::Failed,
                        EventKind::Lifecycle,
                        format!("Task failed with status {exit_status}"),
                    )
                    .await;
                    return;
                }
                Some(Err(error)) => {
                    let _ = capture_task_diff(&state.db, task_id).await;
                    let _ = update_status(
                        &state.db,
                        task_id,
                        TaskStatus::Failed,
                        EventKind::Error,
                        format!("Could not monitor task: {error}"),
                    )
                    .await;
                    return;
                }
                None => {}
            }
        }
    });
}

async fn open_database() -> Result<SqlitePool, String> {
    let db_path = std::env::var("OPENRUNTIME_DB")
        .or_else(|_| std::env::var("MANAGED_AGENTS_DB"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("../data/openruntime.sqlite3"));

    if let Some(parent) = db_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| format!("cannot create data directory: {error}"))?;
    }

    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
        .map_err(|error| format!("invalid sqlite path: {error}"))?
        .create_if_missing(true);

    SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .map_err(|error| format!("cannot open sqlite database: {error}"))
}

async fn init_database(db: &SqlitePool) -> Result<(), String> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS tasks (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            prompt TEXT NOT NULL,
            runner TEXT NOT NULL,
            command TEXT NOT NULL,
            workspace TEXT NOT NULL,
            worktree_path TEXT,
            base_commit TEXT,
            diff_stat TEXT,
            approved_at TEXT,
            worktree_merged_at TEXT,
            worktree_cleaned_at TEXT,
            status TEXT NOT NULL,
            budget_minutes INTEGER NOT NULL,
            policy_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#,
    )
    .execute(db)
    .await
    .map_err(|error| error.to_string())?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS events (
            id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            message TEXT NOT NULL,
            created_at TEXT NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(db)
    .await
    .map_err(|error| error.to_string())?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS workspaces (
            path TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#,
    )
    .execute(db)
    .await
    .map_err(|error| error.to_string())?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_events_task_id ON events(task_id)")
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;

    ensure_column(db, "tasks", "worktree_path", "TEXT").await?;
    ensure_column(db, "tasks", "base_commit", "TEXT").await?;
    ensure_column(db, "tasks", "diff_stat", "TEXT").await?;
    ensure_column(db, "tasks", "approved_at", "TEXT").await?;
    ensure_column(db, "tasks", "worktree_merged_at", "TEXT").await?;
    ensure_column(db, "tasks", "worktree_cleaned_at", "TEXT").await?;

    Ok(())
}

async fn ensure_column(
    db: &SqlitePool,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), String> {
    let pragma = format!("PRAGMA table_info({table})");
    let rows = sqlx::query(&pragma)
        .fetch_all(db)
        .await
        .map_err(|error| error.to_string())?;

    let exists = rows
        .iter()
        .any(|row| row.get::<String, _>("name") == column);

    if !exists {
        let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {definition}");
        sqlx::query(&sql)
            .execute(db)
            .await
            .map_err(|error| error.to_string())?;
    }

    Ok(())
}

async fn mark_orphaned_running_tasks(db: &SqlitePool) -> Result<(), String> {
    let rows = sqlx::query("SELECT id FROM tasks WHERE status = ?")
        .bind(TaskStatus::Running.as_str())
        .fetch_all(db)
        .await
        .map_err(|error| error.to_string())?;

    for row in rows {
        let id = parse_uuid_string(row.get::<String, _>("id"))?;
        update_status(
            db,
            id,
            TaskStatus::Stopped,
            EventKind::Lifecycle,
            "Backend restarted; no live child process was attached".to_string(),
        )
        .await?;
    }

    Ok(())
}

async fn load_task(db: &SqlitePool, id: Uuid) -> Result<Option<Task>, String> {
    let Some(row) = load_task_row(db, id).await? else {
        return Ok(None);
    };
    let events = load_events(db, id).await?;

    Ok(Some(Task {
        id,
        title: row.title,
        prompt: row.prompt,
        runner: RunnerKind::from_str(&row.runner)?,
        command: row.command,
        workspace: row.workspace,
        worktree_path: row.worktree_path,
        base_commit: row.base_commit,
        diff_stat: row.diff_stat,
        approved_at: parse_optional_time(row.approved_at.as_deref())?,
        worktree_merged_at: parse_optional_time(row.worktree_merged_at.as_deref())?,
        worktree_cleaned_at: parse_optional_time(row.worktree_cleaned_at.as_deref())?,
        status: TaskStatus::from_str(&row.status)?,
        budget_minutes: row.budget_minutes.try_into().unwrap_or(15),
        policy: serde_json::from_str(&row.policy_json).unwrap_or_default(),
        created_at: parse_time(&row.created_at)?,
        updated_at: parse_time(&row.updated_at)?,
        events,
    }))
}

async fn load_task_row(db: &SqlitePool, id: Uuid) -> Result<Option<TaskRow>, String> {
    let row = sqlx::query(
        r#"
        SELECT id, title, prompt, runner, command, workspace, status,
               worktree_path, base_commit, diff_stat, approved_at,
               worktree_merged_at, worktree_cleaned_at, budget_minutes,
               policy_json, created_at, updated_at
        FROM tasks
        WHERE id = ?
        "#,
    )
    .bind(id.to_string())
    .fetch_optional(db)
    .await
    .map_err(|error| error.to_string())?;

    Ok(row.map(|row| TaskRow {
        title: row.get("title"),
        prompt: row.get("prompt"),
        runner: row.get("runner"),
        command: row.get("command"),
        workspace: row.get("workspace"),
        worktree_path: row.get("worktree_path"),
        base_commit: row.get("base_commit"),
        diff_stat: row.get("diff_stat"),
        approved_at: row.get("approved_at"),
        worktree_merged_at: row.get("worktree_merged_at"),
        worktree_cleaned_at: row.get("worktree_cleaned_at"),
        status: row.get("status"),
        budget_minutes: row.get("budget_minutes"),
        policy_json: row.get("policy_json"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }))
}

async fn load_events(db: &SqlitePool, task_id: Uuid) -> Result<Vec<TaskEvent>, String> {
    let rows = sqlx::query(
        r#"
        SELECT id, task_id, kind, message, created_at
        FROM events
        WHERE task_id = ?
        ORDER BY created_at ASC
        "#,
    )
    .bind(task_id.to_string())
    .fetch_all(db)
    .await
    .map_err(|error| error.to_string())?;

    rows.into_iter()
        .map(|row| {
            Ok(TaskEvent {
                id: parse_uuid_string(row.get("id"))?,
                task_id: parse_uuid_string(row.get("task_id"))?,
                kind: EventKind::from_str(row.get::<String, _>("kind").as_str())?,
                message: row.get("message"),
                created_at: parse_time(row.get::<String, _>("created_at").as_str())?,
            })
        })
        .collect()
}

async fn update_status(
    db: &SqlitePool,
    task_id: Uuid,
    status: TaskStatus,
    kind: EventKind,
    message: String,
) -> Result<(), String> {
    let now = Utc::now();
    sqlx::query("UPDATE tasks SET status = ?, updated_at = ? WHERE id = ?")
        .bind(status.as_str())
        .bind(now.to_rfc3339())
        .bind(task_id.to_string())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;

    insert_event_at(db, task_id, kind, message, now).await
}

async fn insert_event(
    db: &SqlitePool,
    task_id: Uuid,
    kind: EventKind,
    message: String,
) -> Result<(), String> {
    insert_event_at(db, task_id, kind, message, Utc::now()).await
}

async fn insert_event_at(
    db: &SqlitePool,
    task_id: Uuid,
    kind: EventKind,
    message: String,
    now: DateTime<Utc>,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO events (id, task_id, kind, message, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(task_id.to_string())
    .bind(kind.as_str())
    .bind(message)
    .bind(now.to_rfc3339())
    .execute(db)
    .await
    .map_err(|error| error.to_string())?;

    sqlx::query("UPDATE tasks SET updated_at = ? WHERE id = ?")
        .bind(now.to_rfc3339())
        .bind(task_id.to_string())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;

    Ok(())
}

fn validate_policy(task: &Task) -> Result<(), String> {
    let command = match task.runner {
        RunnerKind::Shell => task.command.to_lowercase(),
        RunnerKind::ClaudeCode | RunnerKind::Codex => task.prompt.to_lowercase(),
    };

    if task.policy.require_approval && task.approved_at.is_none() {
        return Err("Policy requires manual approval before this task can run".to_string());
    }

    for blocked in &task.policy.blocked_commands {
        if !blocked.is_empty() && command.contains(&blocked.to_lowercase()) {
            return Err(format!("Command blocked by policy: {blocked}"));
        }
    }

    if !task.policy.allow_network && contains_any(&command, &["curl", "wget", "ssh", "scp", "nc "])
    {
        return Err("Network access is disabled for this task".to_string());
    }

    if !task.policy.allow_git_write
        && contains_any(
            &command,
            &[
                "git push",
                "git commit",
                "git tag",
                "git merge",
                "git rebase",
            ],
        )
    {
        return Err("Git write operations are disabled for this task".to_string());
    }

    if !task.policy.allow_secrets
        && contains_any(
            &command,
            &[
                ".env",
                "secret",
                "api key",
                "access token",
                "bearer token",
                "keychain",
            ],
        )
    {
        return Err("Secret access is disabled for this task".to_string());
    }

    Ok(())
}

async fn prepare_execution_workspace(db: &SqlitePool, task: &Task) -> Result<String, String> {
    if let Some(worktree_path) = &task.worktree_path {
        if FsPath::new(worktree_path).exists() {
            return Ok(worktree_path.clone());
        }
    }

    if find_executable("git").is_none() {
        insert_event(
            db,
            task.id,
            EventKind::Lifecycle,
            "Git was not found; running without worktree isolation".to_string(),
        )
        .await?;
        return Ok(task.workspace.clone());
    }

    let Ok(root) = git_output_lossy(&task.workspace, &["rev-parse", "--show-toplevel"]).await
    else {
        insert_event(
            db,
            task.id,
            EventKind::Lifecycle,
            "Workspace is not a git repository; running without worktree isolation".to_string(),
        )
        .await?;
        return Ok(task.workspace.clone());
    };

    let repo_root = PathBuf::from(root.trim());
    let selected_workspace = PathBuf::from(&task.workspace);
    let relative_workspace = selected_workspace
        .strip_prefix(&repo_root)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::new());
    let worktree_root = repo_root
        .join(".openruntime")
        .join("worktrees")
        .join(task.id.to_string());

    tokio::fs::create_dir_all(worktree_root.parent().unwrap_or(&repo_root))
        .await
        .map_err(|error| format!("cannot create worktree directory: {error}"))?;

    ensure_local_git_exclude(&repo_root).await?;

    let base_commit = git_output_lossy(&task.workspace, &["rev-parse", "HEAD"])
        .await
        .map(|value| value.trim().to_string())
        .unwrap_or_default();

    if !worktree_root.exists() {
        git_output_lossy(
            repo_root.to_string_lossy().as_ref(),
            &[
                "worktree",
                "add",
                "--detach",
                worktree_root.to_string_lossy().as_ref(),
                "HEAD",
            ],
        )
        .await
        .map_err(|error| format!("cannot create isolated worktree: {error}"))?;
    }

    let worktree_path = worktree_root.to_string_lossy().to_string();
    sqlx::query("UPDATE tasks SET worktree_path = ?, base_commit = ?, updated_at = ? WHERE id = ?")
        .bind(&worktree_path)
        .bind(if base_commit.is_empty() {
            None::<String>
        } else {
            Some(base_commit.clone())
        })
        .bind(Utc::now().to_rfc3339())
        .bind(task.id.to_string())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;

    insert_event(
        db,
        task.id,
        EventKind::Lifecycle,
        format!("Isolated task in git worktree {worktree_path}"),
    )
    .await?;

    Ok(worktree_root
        .join(relative_workspace)
        .to_string_lossy()
        .to_string())
}

async fn capture_task_diff(db: &SqlitePool, task_id: Uuid) -> Result<bool, String> {
    let Some(task) = load_task(db, task_id).await? else {
        return Ok(false);
    };
    let Some(worktree_path) = task.worktree_path else {
        return Ok(false);
    };
    if !FsPath::new(&worktree_path).exists() {
        return Ok(false);
    }

    let stat = git_output_lossy(&worktree_path, &["diff", "--stat", "HEAD", "--"])
        .await
        .unwrap_or_default();
    let status = git_output_lossy(&worktree_path, &["status", "--short"])
        .await
        .unwrap_or_default();
    let summary = [status.trim(), stat.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    sqlx::query("UPDATE tasks SET diff_stat = ?, updated_at = ? WHERE id = ?")
        .bind(if summary.is_empty() {
            None::<String>
        } else {
            Some(summary.clone())
        })
        .bind(Utc::now().to_rfc3339())
        .bind(task_id.to_string())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;

    let has_changes = !summary.is_empty();

    if summary.is_empty() {
        insert_event(
            db,
            task_id,
            EventKind::Diff,
            "No file changes detected in isolated worktree".to_string(),
        )
        .await?;
    } else {
        insert_event(db, task_id, EventKind::Diff, summary).await?;
    }

    Ok(has_changes)
}

async fn ensure_local_git_exclude(repo_root: &FsPath) -> Result<(), String> {
    let exclude_path = repo_root.join(".git").join("info").join("exclude");
    let marker = ".openruntime/";
    let existing = tokio::fs::read_to_string(&exclude_path)
        .await
        .unwrap_or_default();

    if existing.lines().any(|line| line.trim() == marker) {
        return Ok(());
    }

    let next = if existing.ends_with('\n') || existing.is_empty() {
        format!("{existing}{marker}\n")
    } else {
        format!("{existing}\n{marker}\n")
    };

    tokio::fs::write(exclude_path, next)
        .await
        .map_err(|error| format!("cannot update local git exclude: {error}"))
}

async fn git_output_lossy(cwd: &str, args: &[&str]) -> Result<String, String> {
    git_output_lossy_allowing(cwd, args, &[0]).await
}

async fn git_output_lossy_allowing(
    cwd: &str,
    args: &[&str],
    allowed_codes: &[i32],
) -> Result<String, String> {
    let git = find_executable("git").unwrap_or_else(|| PathBuf::from("git"));
    let output = Command::new(git)
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|error| format!("git failed to start: {error}"))?;

    let code = output.status.code().unwrap_or(-1);
    if !allowed_codes.contains(&code) {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("git exited with {}", output.status)
        } else {
            stderr
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn untracked_patch(worktree_path: &str) -> Result<String, String> {
    let untracked = git_output_lossy(
        worktree_path,
        &["ls-files", "--others", "--exclude-standard"],
    )
    .await?;
    let mut patches = Vec::new();

    for file in untracked
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let path = PathBuf::from(worktree_path).join(file);
        if !path.is_file() {
            continue;
        }

        let patch = git_output_lossy_allowing(
            worktree_path,
            &["diff", "--no-index", "--", "/dev/null", file],
            &[0, 1],
        )
        .await
        .unwrap_or_else(|error| {
            format!("Untracked file: {file}\nCould not render patch: {error}\n")
        });
        patches.push(patch);
    }

    Ok(patches.join("\n"))
}

async fn apply_patch_to_target(repo_root: &str, patch: &str) -> Result<(), String> {
    if patch.trim().is_empty() {
        return Ok(());
    }

    let patch_dir = PathBuf::from(repo_root)
        .join(".openruntime")
        .join("patches");
    tokio::fs::create_dir_all(&patch_dir)
        .await
        .map_err(|error| format!("cannot create patch directory: {error}"))?;
    let patch_path = patch_dir.join(format!("{}.patch", Uuid::new_v4()));
    tokio::fs::write(&patch_path, patch)
        .await
        .map_err(|error| format!("cannot write patch file: {error}"))?;

    let result = git_output_lossy(
        repo_root,
        &[
            "apply",
            "--whitespace=nowarn",
            patch_path.to_string_lossy().as_ref(),
        ],
    )
    .await;
    let _ = tokio::fs::remove_file(&patch_path).await;
    result.map(|_| ())
}

async fn copy_worktree_file(
    source_root: &FsPath,
    target_root: &FsPath,
    relative_file: &str,
) -> Result<(), String> {
    let source = source_root.join(relative_file);
    let target = target_root.join(relative_file);

    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| format!("cannot create target directory: {error}"))?;
    }

    tokio::fs::copy(&source, &target)
        .await
        .map(|_| ())
        .map_err(|error| {
            format!(
                "cannot copy {} to {}: {error}",
                source.display(),
                target.display()
            )
        })
}

fn build_runner_command(
    task: &Task,
    execution_workspace: &str,
) -> Result<(&'static str, Command), String> {
    match task.runner {
        RunnerKind::Shell => {
            let mut command = Command::new("/bin/sh");
            command
                .arg("-lc")
                .arg(&task.command)
                .current_dir(execution_workspace);
            Ok(("Shell", command))
        }
        RunnerKind::Codex => {
            let codex = find_executable("codex").ok_or_else(|| {
                "Codex CLI was not found in PATH. Install/login Codex first.".to_string()
            })?;
            let mut command = Command::new(codex);
            command
                .arg("exec")
                .arg("--json")
                .arg("--skip-git-repo-check")
                .arg("-s")
                .arg("workspace-write")
                .arg("-C")
                .arg(execution_workspace)
                .arg(&task.prompt)
                .current_dir(execution_workspace);
            Ok(("Codex", command))
        }
        RunnerKind::ClaudeCode => {
            let claude = find_executable("claude").ok_or_else(|| {
                "Claude Code CLI was not found in PATH. Install Claude Code or add `claude` to PATH."
                    .to_string()
            })?;
            let mut command = Command::new(claude);
            command
                .arg("-p")
                .arg(&task.prompt)
                .current_dir(execution_workspace);
            Ok(("Claude Code", command))
        }
    }
}

fn normalize_command(
    runner: &RunnerKind,
    command: Option<String>,
    prompt: &str,
) -> Result<String, (StatusCode, String)> {
    let command = command.unwrap_or_default().trim().to_string();

    match runner {
        RunnerKind::Shell if command.is_empty() => {
            Err((StatusCode::BAD_REQUEST, "command is required".to_string()))
        }
        RunnerKind::Shell => Ok(command),
        RunnerKind::Codex | RunnerKind::ClaudeCode if prompt.is_empty() => {
            Err((StatusCode::BAD_REQUEST, "goal is required".to_string()))
        }
        RunnerKind::Codex => Ok(
            "codex exec --json --skip-git-repo-check -s workspace-write -C <workspace> <goal>"
                .to_string(),
        ),
        RunnerKind::ClaudeCode => Ok("claude -p <goal>".to_string()),
    }
}

fn find_executable(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }

    env::var_os("PATH")
        .unwrap_or_else(|| OsString::from(""))
        .to_string_lossy()
        .split(':')
        .map(|dir| FsPath::new(dir).join(name))
        .find(|path| path.is_file())
}

fn contains_any(command: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| command.contains(needle))
}

fn resolve_workspace(workspace: Option<String>) -> Result<String, (StatusCode, String)> {
    let raw = workspace.unwrap_or_else(|| ".".to_string());
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map_err(|error| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("cannot resolve current directory: {error}"),
                )
            })?
            .join(path)
    };

    if !path.exists() {
        return Err((
            StatusCode::BAD_REQUEST,
            "workspace does not exist".to_string(),
        ));
    }

    if !path.is_dir() {
        return Err((
            StatusCode::BAD_REQUEST,
            "workspace must be a directory".to_string(),
        ));
    }

    path.canonicalize()
        .map(|path| path.to_string_lossy().to_string())
        .map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                format!("cannot resolve workspace: {error}"),
            )
        })
}

fn push_workspace_project(projects: &mut Vec<WorkspaceProject>, path: PathBuf) {
    let Ok(path) = path.canonicalize() else {
        return;
    };
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return;
    };

    let path = path.to_string_lossy().to_string();
    if projects.iter().any(|project| project.path == path) {
        return;
    }

    projects.push(WorkspaceProject {
        name: name.to_string(),
        path,
    });
}

fn workspace_project_from_path(path: PathBuf) -> Option<WorkspaceProject> {
    let path = path.canonicalize().ok()?;
    let name = path.file_name()?.to_str()?.to_string();
    Some(WorkspaceProject {
        name,
        path: path.to_string_lossy().to_string(),
    })
}

fn parse_uuid(value: &str) -> Result<Uuid, (StatusCode, String)> {
    Uuid::parse_str(value).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("bad task id: {error}"),
        )
    })
}

fn parse_uuid_string(value: String) -> Result<Uuid, String> {
    Uuid::parse_str(&value).map_err(|error| format!("bad uuid {value}: {error}"))
}

fn parse_time(value: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| format!("bad timestamp {value}: {error}"))
}

fn parse_optional_time(value: Option<&str>) -> Result<Option<DateTime<Utc>>, String> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(parse_time)
        .transpose()
}

fn internal_error(error: impl ToString) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

struct TaskRow {
    title: String,
    prompt: String,
    runner: String,
    command: String,
    workspace: String,
    worktree_path: Option<String>,
    base_commit: Option<String>,
    diff_stat: Option<String>,
    approved_at: Option<String>,
    worktree_merged_at: Option<String>,
    worktree_cleaned_at: Option<String>,
    status: String,
    budget_minutes: i64,
    policy_json: String,
    created_at: String,
    updated_at: String,
}
