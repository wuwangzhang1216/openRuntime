use crate::models::{
    CostLedger, EventKind, RunnerKind, Task, TaskAttempt, TaskEvent, TaskPolicy, TaskRow,
    TaskStatus,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{path::PathBuf, str::FromStr};
use uuid::Uuid;

pub async fn open_database() -> Result<SqlitePool, String> {
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

pub async fn init_database(db: &SqlitePool) -> Result<(), String> {
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
            execution_workspace TEXT,
            runner_session_id TEXT,
            base_commit TEXT,
            diff_stat TEXT,
            approved_at TEXT,
            worktree_merged_at TEXT,
            worktree_cleaned_at TEXT,
            status TEXT NOT NULL,
            budget_minutes INTEGER NOT NULL,
            policy_json TEXT NOT NULL,
            effective_policy_json TEXT,
            cost_ledger_json TEXT,
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
        CREATE TABLE IF NOT EXISTS task_attempts (
            id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL,
            attempt_number INTEGER NOT NULL,
            runner TEXT NOT NULL,
            status TEXT NOT NULL,
            execution_workspace TEXT,
            runner_session_id TEXT,
            started_at TEXT NOT NULL,
            finished_at TEXT,
            exit_status TEXT,
            summary TEXT,
            FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE CASCADE
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
            attempt_id TEXT,
            kind TEXT NOT NULL,
            message TEXT NOT NULL,
            metadata_json TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE CASCADE,
            FOREIGN KEY(attempt_id) REFERENCES task_attempts(id) ON DELETE SET NULL
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

    ensure_column(db, "tasks", "worktree_path", "TEXT").await?;
    ensure_column(db, "tasks", "execution_workspace", "TEXT").await?;
    ensure_column(db, "tasks", "runner_session_id", "TEXT").await?;
    ensure_column(db, "tasks", "base_commit", "TEXT").await?;
    ensure_column(db, "tasks", "diff_stat", "TEXT").await?;
    ensure_column(db, "tasks", "approved_at", "TEXT").await?;
    ensure_column(db, "tasks", "worktree_merged_at", "TEXT").await?;
    ensure_column(db, "tasks", "worktree_cleaned_at", "TEXT").await?;
    ensure_column(db, "tasks", "effective_policy_json", "TEXT").await?;
    ensure_column(db, "tasks", "cost_ledger_json", "TEXT").await?;
    ensure_column(db, "events", "attempt_id", "TEXT").await?;
    ensure_column(db, "events", "metadata_json", "TEXT").await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_events_task_id ON events(task_id)")
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_events_attempt_id ON events(attempt_id)")
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_task_attempts_task_id ON task_attempts(task_id)")
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;

    Ok(())
}

pub async fn create_task(
    db: &SqlitePool,
    title: &str,
    prompt: &str,
    runner: RunnerKind,
    command: String,
    workspace: String,
    budget_minutes: u32,
    policy: TaskPolicy,
) -> Result<Task, String> {
    let id = Uuid::new_v4();
    let now = Utc::now();
    let policy_json = serde_json::to_string(&policy).map_err(|error| error.to_string())?;
    let cost_ledger_json =
        serde_json::to_string(&CostLedger::default()).map_err(|error| error.to_string())?;

    sqlx::query(
        r#"
        INSERT INTO tasks (
            id, title, prompt, runner, command, workspace, status,
            budget_minutes, policy_json, cost_ledger_json, created_at, updated_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(id.to_string())
    .bind(title)
    .bind(prompt)
    .bind(runner.as_str())
    .bind(command)
    .bind(workspace)
    .bind(TaskStatus::Queued.as_str())
    .bind(i64::from(budget_minutes))
    .bind(policy_json)
    .bind(cost_ledger_json)
    .bind(now.to_rfc3339())
    .bind(now.to_rfc3339())
    .execute(db)
    .await
    .map_err(|error| error.to_string())?;

    insert_event(db, id, EventKind::Lifecycle, "Task created".to_string()).await?;

    load_task(db, id)
        .await?
        .ok_or_else(|| "task disappeared after create".to_string())
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

pub async fn mark_orphaned_running_tasks(db: &SqlitePool) -> Result<(), String> {
    let rows = sqlx::query("SELECT id FROM tasks WHERE status = ?")
        .bind(TaskStatus::Running.as_str())
        .fetch_all(db)
        .await
        .map_err(|error| error.to_string())?;

    for row in rows {
        let id = parse_uuid_string(row.get::<String, _>("id"))?;
        finish_open_attempts(
            db,
            id,
            TaskStatus::Stopped,
            "Backend restarted; no live child process was attached".to_string(),
        )
        .await?;
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

pub async fn list_tasks(db: &SqlitePool) -> Result<Vec<Task>, String> {
    let ids = sqlx::query("SELECT id FROM tasks ORDER BY updated_at DESC")
        .fetch_all(db)
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|row| row.get::<String, _>("id"))
        .collect::<Vec<_>>();

    let mut tasks = Vec::with_capacity(ids.len());
    for id in ids {
        let id = parse_uuid_string(id)?;
        if let Some(task) = load_task(db, id).await? {
            tasks.push(task);
        }
    }

    Ok(tasks)
}

pub async fn load_task(db: &SqlitePool, id: Uuid) -> Result<Option<Task>, String> {
    let Some(row) = load_task_row(db, id).await? else {
        return Ok(None);
    };
    let events = load_events(db, id).await?;
    let attempts = load_attempts(db, id).await?;
    let current_attempt = attempts.last().cloned();

    Ok(Some(Task {
        id,
        title: row.title,
        prompt: row.prompt,
        runner: RunnerKind::from_str(&row.runner)?,
        command: row.command,
        workspace: row.workspace,
        worktree_path: row.worktree_path,
        execution_workspace: row.execution_workspace,
        runner_session_id: row.runner_session_id,
        base_commit: row.base_commit,
        diff_stat: row.diff_stat,
        approved_at: parse_optional_time(row.approved_at.as_deref())?,
        worktree_merged_at: parse_optional_time(row.worktree_merged_at.as_deref())?,
        worktree_cleaned_at: parse_optional_time(row.worktree_cleaned_at.as_deref())?,
        status: TaskStatus::from_str(&row.status)?,
        budget_minutes: row.budget_minutes.try_into().unwrap_or(15),
        policy: serde_json::from_str(&row.policy_json).unwrap_or_default(),
        effective_policy: row
            .effective_policy_json
            .as_deref()
            .and_then(|value| serde_json::from_str(value).ok()),
        cost_ledger: row
            .cost_ledger_json
            .as_deref()
            .and_then(|value| serde_json::from_str(value).ok())
            .unwrap_or_default(),
        created_at: parse_time(&row.created_at)?,
        updated_at: parse_time(&row.updated_at)?,
        events,
        attempts,
        current_attempt,
    }))
}

pub async fn load_attempts(db: &SqlitePool, task_id: Uuid) -> Result<Vec<TaskAttempt>, String> {
    let rows = sqlx::query(
        r#"
        SELECT id, task_id, attempt_number, runner, status, execution_workspace,
               runner_session_id, started_at, finished_at, exit_status, summary
        FROM task_attempts
        WHERE task_id = ?
        ORDER BY attempt_number ASC
        "#,
    )
    .bind(task_id.to_string())
    .fetch_all(db)
    .await
    .map_err(|error| error.to_string())?;

    rows.into_iter()
        .map(|row| {
            let attempt_number: i64 = row.get("attempt_number");
            Ok(TaskAttempt {
                id: parse_uuid_string(row.get("id"))?,
                task_id: parse_uuid_string(row.get("task_id"))?,
                attempt_number: attempt_number.try_into().unwrap_or(0),
                runner: RunnerKind::from_str(row.get::<String, _>("runner").as_str())?,
                status: TaskStatus::from_str(row.get::<String, _>("status").as_str())?,
                execution_workspace: row.get("execution_workspace"),
                runner_session_id: row.get("runner_session_id"),
                started_at: parse_time(row.get::<String, _>("started_at").as_str())?,
                finished_at: parse_optional_time(
                    row.get::<Option<String>, _>("finished_at").as_deref(),
                )?,
                exit_status: row.get("exit_status"),
                summary: row.get("summary"),
            })
        })
        .collect()
}

pub async fn load_task_row(db: &SqlitePool, id: Uuid) -> Result<Option<TaskRow>, String> {
    let row = sqlx::query(
        r#"
        SELECT id, title, prompt, runner, command, workspace, status,
               worktree_path, execution_workspace, runner_session_id, base_commit, diff_stat, approved_at,
               worktree_merged_at, worktree_cleaned_at, budget_minutes,
               policy_json, effective_policy_json, cost_ledger_json, created_at, updated_at
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
        execution_workspace: row.get("execution_workspace"),
        runner_session_id: row.get("runner_session_id"),
        base_commit: row.get("base_commit"),
        diff_stat: row.get("diff_stat"),
        approved_at: row.get("approved_at"),
        worktree_merged_at: row.get("worktree_merged_at"),
        worktree_cleaned_at: row.get("worktree_cleaned_at"),
        status: row.get("status"),
        budget_minutes: row.get("budget_minutes"),
        policy_json: row.get("policy_json"),
        effective_policy_json: row.get("effective_policy_json"),
        cost_ledger_json: row.get("cost_ledger_json"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }))
}

pub async fn load_events(db: &SqlitePool, task_id: Uuid) -> Result<Vec<TaskEvent>, String> {
    let rows = sqlx::query(
        r#"
        SELECT id, task_id, attempt_id, kind, message, metadata_json, created_at
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
                attempt_id: row
                    .get::<Option<String>, _>("attempt_id")
                    .map(parse_uuid_string)
                    .transpose()?,
                kind: EventKind::from_str(row.get::<String, _>("kind").as_str())?,
                message: row.get("message"),
                metadata: row
                    .get::<Option<String>, _>("metadata_json")
                    .as_deref()
                    .and_then(|value| serde_json::from_str(value).ok())
                    .unwrap_or(Value::Null),
                created_at: parse_time(row.get::<String, _>("created_at").as_str())?,
            })
        })
        .collect()
}

pub async fn create_attempt(
    db: &SqlitePool,
    task: &Task,
    execution_workspace: &str,
) -> Result<TaskAttempt, String> {
    let row = sqlx::query("SELECT COALESCE(MAX(attempt_number), 0) + 1 AS next_number FROM task_attempts WHERE task_id = ?")
        .bind(task.id.to_string())
        .fetch_one(db)
        .await
        .map_err(|error| error.to_string())?;
    let attempt_number: i64 = row.get("next_number");
    let id = Uuid::new_v4();
    let now = Utc::now();

    sqlx::query(
        r#"
        INSERT INTO task_attempts (
            id, task_id, attempt_number, runner, status, execution_workspace,
            runner_session_id, started_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(id.to_string())
    .bind(task.id.to_string())
    .bind(attempt_number)
    .bind(task.runner.as_str())
    .bind(TaskStatus::Running.as_str())
    .bind(execution_workspace)
    .bind(task.runner_session_id.clone())
    .bind(now.to_rfc3339())
    .execute(db)
    .await
    .map_err(|error| error.to_string())?;

    Ok(TaskAttempt {
        id,
        task_id: task.id,
        attempt_number: attempt_number.try_into().unwrap_or(0),
        runner: task.runner.clone(),
        status: TaskStatus::Running,
        execution_workspace: Some(execution_workspace.to_string()),
        runner_session_id: task.runner_session_id.clone(),
        started_at: now,
        finished_at: None,
        exit_status: None,
        summary: None,
    })
}

pub async fn finish_attempt(
    db: &SqlitePool,
    attempt_id: Uuid,
    status: TaskStatus,
    exit_status: Option<String>,
    summary: Option<String>,
) -> Result<(), String> {
    sqlx::query(
        r#"
        UPDATE task_attempts
        SET status = ?, finished_at = ?, exit_status = ?, summary = ?
        WHERE id = ?
        "#,
    )
    .bind(status.as_str())
    .bind(Utc::now().to_rfc3339())
    .bind(exit_status)
    .bind(summary)
    .bind(attempt_id.to_string())
    .execute(db)
    .await
    .map_err(|error| error.to_string())?;
    Ok(())
}

pub async fn mark_attempt_status(
    db: &SqlitePool,
    attempt_id: Uuid,
    status: TaskStatus,
    summary: Option<String>,
) -> Result<(), String> {
    sqlx::query("UPDATE task_attempts SET status = ?, summary = ? WHERE id = ?")
        .bind(status.as_str())
        .bind(summary)
        .bind(attempt_id.to_string())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub async fn finish_open_attempts(
    db: &SqlitePool,
    task_id: Uuid,
    status: TaskStatus,
    summary: String,
) -> Result<(), String> {
    sqlx::query(
        r#"
        UPDATE task_attempts
        SET status = ?, finished_at = ?, summary = ?
        WHERE task_id = ? AND finished_at IS NULL
        "#,
    )
    .bind(status.as_str())
    .bind(Utc::now().to_rfc3339())
    .bind(summary)
    .bind(task_id.to_string())
    .execute(db)
    .await
    .map_err(|error| error.to_string())?;
    Ok(())
}

pub async fn load_runner_session_events(
    db: &SqlitePool,
    task_id: Uuid,
) -> Result<Vec<TaskEvent>, String> {
    Ok(load_events(db, task_id)
        .await?
        .into_iter()
        .filter(|event| is_runner_session_event(&event.kind, &event.message))
        .collect())
}

pub(crate) fn is_runner_session_event(kind: &EventKind, message: &str) -> bool {
    match kind {
        EventKind::Stdout | EventKind::Stderr => true,
        EventKind::Lifecycle => {
            let lower = message.to_lowercase();
            lower.contains("runner") || lower.contains("session") || lower.starts_with("starting ")
        }
        EventKind::Diff | EventKind::Input | EventKind::Error => false,
    }
}

pub async fn approve_task(
    db: &SqlitePool,
    task_id: Uuid,
    note: Option<&str>,
) -> Result<(), String> {
    let now = Utc::now();
    sqlx::query("UPDATE tasks SET approved_at = ?, updated_at = ? WHERE id = ?")
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(task_id.to_string())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;

    let note = note
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!(": {value}"))
        .unwrap_or_default();
    insert_event(
        db,
        task_id,
        EventKind::Input,
        format!("Task approved{note}"),
    )
    .await
}

pub async fn set_worktree(
    db: &SqlitePool,
    task_id: Uuid,
    worktree_path: &str,
    base_commit: Option<String>,
) -> Result<(), String> {
    sqlx::query("UPDATE tasks SET worktree_path = ?, base_commit = ?, updated_at = ? WHERE id = ?")
        .bind(worktree_path)
        .bind(base_commit)
        .bind(Utc::now().to_rfc3339())
        .bind(task_id.to_string())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub async fn set_runner_session_id(
    db: &SqlitePool,
    task_id: Uuid,
    runner_session_id: &str,
) -> Result<(), String> {
    sqlx::query("UPDATE tasks SET runner_session_id = ?, updated_at = ? WHERE id = ?")
        .bind(runner_session_id)
        .bind(Utc::now().to_rfc3339())
        .bind(task_id.to_string())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub async fn set_attempt_runner_session_id(
    db: &SqlitePool,
    attempt_id: Uuid,
    runner_session_id: &str,
) -> Result<(), String> {
    sqlx::query("UPDATE task_attempts SET runner_session_id = ? WHERE id = ?")
        .bind(runner_session_id)
        .bind(attempt_id.to_string())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub async fn set_execution_workspace(
    db: &SqlitePool,
    task_id: Uuid,
    execution_workspace: &str,
) -> Result<(), String> {
    sqlx::query("UPDATE tasks SET execution_workspace = ?, updated_at = ? WHERE id = ?")
        .bind(execution_workspace)
        .bind(Utc::now().to_rfc3339())
        .bind(task_id.to_string())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub async fn set_effective_policy(
    db: &SqlitePool,
    task_id: Uuid,
    policy: &TaskPolicy,
) -> Result<(), String> {
    let policy_json = serde_json::to_string(policy).map_err(|error| error.to_string())?;
    sqlx::query(
        r#"
        UPDATE tasks
        SET effective_policy_json = COALESCE(effective_policy_json, ?), updated_at = ?
        WHERE id = ?
        "#,
    )
    .bind(policy_json)
    .bind(Utc::now().to_rfc3339())
    .bind(task_id.to_string())
    .execute(db)
    .await
    .map_err(|error| error.to_string())?;
    Ok(())
}

pub async fn set_diff_stat(
    db: &SqlitePool,
    task_id: Uuid,
    diff_stat: Option<String>,
) -> Result<(), String> {
    sqlx::query("UPDATE tasks SET diff_stat = ?, updated_at = ? WHERE id = ?")
        .bind(diff_stat)
        .bind(Utc::now().to_rfc3339())
        .bind(task_id.to_string())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub async fn mark_worktree_merged(db: &SqlitePool, task_id: Uuid) -> Result<(), String> {
    let now = Utc::now();
    sqlx::query("UPDATE tasks SET worktree_merged_at = ?, updated_at = ? WHERE id = ?")
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(task_id.to_string())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub async fn mark_worktree_cleaned(db: &SqlitePool, task_id: Uuid) -> Result<(), String> {
    let now = Utc::now();
    sqlx::query("UPDATE tasks SET worktree_cleaned_at = ?, updated_at = ? WHERE id = ?")
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(task_id.to_string())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub async fn update_status(
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

pub async fn update_status_for_attempt(
    db: &SqlitePool,
    task_id: Uuid,
    attempt_id: Option<Uuid>,
    status: TaskStatus,
    kind: EventKind,
    message: String,
    metadata: Value,
) -> Result<(), String> {
    let now = Utc::now();
    sqlx::query("UPDATE tasks SET status = ?, updated_at = ? WHERE id = ?")
        .bind(status.as_str())
        .bind(now.to_rfc3339())
        .bind(task_id.to_string())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;

    insert_event_at_with_metadata(db, task_id, attempt_id, kind, message, metadata, now).await
}

pub async fn insert_event(
    db: &SqlitePool,
    task_id: Uuid,
    kind: EventKind,
    message: String,
) -> Result<(), String> {
    insert_event_at(db, task_id, kind, message, Utc::now()).await
}

pub async fn insert_event_for_attempt(
    db: &SqlitePool,
    task_id: Uuid,
    attempt_id: Option<Uuid>,
    kind: EventKind,
    message: String,
    metadata: Value,
) -> Result<(), String> {
    insert_event_at_with_metadata(db, task_id, attempt_id, kind, message, metadata, Utc::now())
        .await
}

pub async fn insert_event_at(
    db: &SqlitePool,
    task_id: Uuid,
    kind: EventKind,
    message: String,
    now: DateTime<Utc>,
) -> Result<(), String> {
    insert_event_at_with_metadata(db, task_id, None, kind, message, Value::Null, now).await
}

pub async fn insert_event_at_with_metadata(
    db: &SqlitePool,
    task_id: Uuid,
    attempt_id: Option<Uuid>,
    kind: EventKind,
    message: String,
    metadata: Value,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let metadata_json = (!metadata.is_null())
        .then(|| serde_json::to_string(&metadata))
        .transpose()
        .map_err(|error| error.to_string())?;
    sqlx::query(
        r#"
        INSERT INTO events (id, task_id, attempt_id, kind, message, metadata_json, created_at)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(Uuid::new_v4().to_string())
    .bind(task_id.to_string())
    .bind(attempt_id.map(|id| id.to_string()))
    .bind(kind.as_str())
    .bind(message)
    .bind(metadata_json)
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

pub async fn add_cost_delta(
    db: &SqlitePool,
    task_id: Uuid,
    delta: CostLedger,
) -> Result<(), String> {
    if delta == CostLedger::default() {
        return Ok(());
    }

    let Some(mut task) = load_task(db, task_id).await? else {
        return Ok(());
    };
    task.cost_ledger.runtime_millis += delta.runtime_millis;
    task.cost_ledger.input_tokens += delta.input_tokens;
    task.cost_ledger.output_tokens += delta.output_tokens;
    task.cost_ledger.tool_calls += delta.tool_calls;
    task.cost_ledger.estimated_cents += delta.estimated_cents;

    let json = serde_json::to_string(&task.cost_ledger).map_err(|error| error.to_string())?;
    sqlx::query("UPDATE tasks SET cost_ledger_json = ?, updated_at = ? WHERE id = ?")
        .bind(json)
        .bind(Utc::now().to_rfc3339())
        .bind(task_id.to_string())
        .execute(db)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn resolve_workspace(
    workspace: Option<String>,
) -> Result<String, (axum::http::StatusCode, String)> {
    let raw = workspace.unwrap_or_else(|| ".".to_string());
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map_err(|error| {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("cannot resolve current directory: {error}"),
                )
            })?
            .join(path)
    };

    if !path.exists() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "workspace does not exist".to_string(),
        ));
    }

    if !path.is_dir() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "workspace must be a directory".to_string(),
        ));
    }

    path.canonicalize()
        .map(|path| path.to_string_lossy().to_string())
        .map_err(|error| {
            (
                axum::http::StatusCode::BAD_REQUEST,
                format!("cannot resolve workspace: {error}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_session_event_filter_keeps_output_and_session_lifecycle_only() {
        assert!(is_runner_session_event(&EventKind::Stdout, "anything"));
        assert!(is_runner_session_event(&EventKind::Stderr, "anything"));
        assert!(is_runner_session_event(
            &EventKind::Lifecycle,
            "Runner session id: abc"
        ));
        assert!(is_runner_session_event(
            &EventKind::Lifecycle,
            "Starting Claude Code runner in /tmp/work"
        ));

        assert!(!is_runner_session_event(
            &EventKind::Lifecycle,
            "Task approved"
        ));
        assert!(!is_runner_session_event(
            &EventKind::Input,
            "User reply: ok"
        ));
        assert!(!is_runner_session_event(
            &EventKind::Error,
            "policy blocked"
        ));
    }
}
