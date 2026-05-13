mod models;
mod policy_engine;
mod runner_adapters;
mod supervisor;
mod task_store;
mod worktree_review;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use chrono::Utc;
use models::{
    ApproveTaskRequest, HealthResponse, RegisterWorkspaceRequest, ReplyTaskRequest,
    RunnerSessionLogsResponse, RunnerSessionResponse, Task, TaskStatus, WorkspaceProject,
    WorktreeActionResponse,
};
use sqlx::{Row, SqlitePool};
use std::{net::SocketAddr, path::PathBuf};
use supervisor::Supervisor;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    supervisor: Supervisor,
}

type ApiResult<T> = Result<Json<T>, (StatusCode, String)>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("openruntime_backend=debug,tower_http=debug")
        .init();

    let db = task_store::open_database()
        .await
        .expect("open sqlite database");
    task_store::init_database(&db)
        .await
        .expect("initialize sqlite database");
    task_store::mark_orphaned_running_tasks(&db)
        .await
        .expect("recover task state");

    let state = AppState {
        db,
        supervisor: Supervisor::default(),
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
        .route("/tasks/{id}/runner/logs", get(get_runner_logs))
        .route("/tasks/{id}/runner/attach", post(attach_runner_session))
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

async fn list_runners() -> Json<Vec<models::RunnerInfo>> {
    Json(runner_adapters::list_runners())
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
    let path = task_store::resolve_workspace(Some(payload.path))?;
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
    let output = tokio::process::Command::new("osascript")
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
    task_store::list_tasks(&state.db)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn create_task(
    State(state): State<AppState>,
    Json(payload): Json<models::CreateTaskRequest>,
) -> ApiResult<Task> {
    if payload.title.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "title is required".to_string()));
    }

    let workspace = task_store::resolve_workspace(payload.workspace)?;
    let prompt = payload.prompt.trim().to_string();
    let command = runner_adapters::normalize_command(&payload.runner, payload.command, &prompt)?;
    let policy = payload.policy.unwrap_or_default();

    if let Err(message) = policy_engine::validate_task_plan(
        &payload.runner,
        &command,
        &prompt,
        &workspace,
        &policy,
        Some(()),
    ) {
        return Err((StatusCode::FORBIDDEN, message));
    }

    task_store::create_task(
        &state.db,
        payload.title.trim(),
        &prompt,
        payload.runner,
        command,
        workspace,
        payload.budget_minutes.unwrap_or(15).clamp(1, 240),
        policy,
    )
    .await
    .map(Json)
    .map_err(internal_error)
}

async fn get_task(State(state): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Task> {
    task_store::load_task(&state.db, id)
        .await
        .map_err(internal_error)?
        .map(Json)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "task not found".to_string()))
}

async fn list_events(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Vec<models::TaskEvent>> {
    if task_store::load_task_row(&state.db, id)
        .await
        .map_err(internal_error)?
        .is_none()
    {
        return Err((StatusCode::NOT_FOUND, "task not found".to_string()));
    }

    task_store::load_events(&state.db, id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_task_diff(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<models::TaskDiffResponse> {
    worktree_review::get_task_diff(&state.db, id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn get_runner_logs(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<RunnerSessionLogsResponse> {
    let task = task_store::load_task(&state.db, id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "task not found".to_string()))?;
    let events = task_store::load_events(&state.db, id)
        .await
        .map_err(internal_error)?;
    let message = if task.runner_session_id.is_some() {
        "Returning persisted runner event stream for this session".to_string()
    } else {
        "This task does not have a runner session id yet".to_string()
    };

    Ok(Json(RunnerSessionLogsResponse {
        task_id: id,
        runner: task.runner,
        session_id: task.runner_session_id,
        events,
        message,
    }))
}

async fn attach_runner_session(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<RunnerSessionResponse> {
    let task = task_store::load_task(&state.db, id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "task not found".to_string()))?;
    let command = runner_adapters::attach_command_display(&task);
    let supported = command.is_some();
    let message = if supported {
        "Use this command to attach to the underlying runner session in a terminal".to_string()
    } else if task.runner_session_id.is_none() {
        "This task does not have a runner session id yet".to_string()
    } else {
        "This runner does not expose an attach command".to_string()
    };

    Ok(Json(RunnerSessionResponse {
        task_id: id,
        runner: task.runner,
        session_id: task.runner_session_id,
        supported,
        command,
        message,
    }))
}

async fn approve_task(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<ApproveTaskRequest>,
) -> ApiResult<Task> {
    let task = task_store::load_task(&state.db, id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "task not found".to_string()))?;

    task_store::approve_task(&state.db, id, payload.note.as_deref())
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

    state
        .supervisor
        .reply(&state.db, id, &message)
        .await
        .map_err(internal_error)?;
    get_task(State(state), Path(id)).await
}

async fn start_task(State(state): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Task> {
    state
        .supervisor
        .start(&state.db, id)
        .await
        .map_err(|message| (StatusCode::PRECONDITION_FAILED, message))?;
    get_task(State(state), Path(id)).await
}

async fn stop_task(State(state): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Task> {
    state
        .supervisor
        .stop(&state.db, id)
        .await
        .map_err(internal_error)?;
    get_task(State(state), Path(id)).await
}

async fn merge_task_worktree(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<WorktreeActionResponse> {
    worktree_review::merge_task_worktree(&state.db, id)
        .await
        .map(Json)
        .map_err(internal_error)
}

async fn cleanup_task_worktree(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<WorktreeActionResponse> {
    worktree_review::cleanup_task_worktree(&state.db, id)
        .await
        .map(Json)
        .map_err(internal_error)
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

fn internal_error(error: impl ToString) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}
