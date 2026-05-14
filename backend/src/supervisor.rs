use crate::{
    models::{CostLedger, EventKind, TaskStatus},
    policy_engine, runner_adapters, task_store, worktree_review,
};
use sqlx::SqlitePool;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin},
    sync::Mutex,
};
use uuid::Uuid;

#[derive(Clone, Default)]
pub struct Supervisor {
    children: Arc<Mutex<HashMap<Uuid, RunningProcess>>>,
}

struct RunningProcess {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Supervisor {
    pub async fn start(&self, db: &SqlitePool, id: Uuid) -> Result<(), String> {
        let mut task = task_store::load_task(db, id)
            .await?
            .ok_or_else(|| "task not found".to_string())?;

        if task.status == TaskStatus::Running {
            return Err("task is already running".to_string());
        }

        if task.effective_policy.is_none() {
            task_store::set_effective_policy(db, id, &task.policy).await?;
            task_store::insert_event(
                db,
                id,
                EventKind::Lifecycle,
                "Effective policy snapshot frozen for this task".to_string(),
            )
            .await?;
            task = task_store::load_task(db, id).await?.unwrap_or(task);
        }

        if let Err(message) = policy_engine::validate_task(&task) {
            task_store::update_status(
                db,
                id,
                TaskStatus::NeedsInput,
                EventKind::Error,
                message.clone(),
            )
            .await?;
            return Err(message);
        }

        let execution_workspace = worktree_review::prepare_execution_workspace(db, id).await?;
        task_store::set_execution_workspace(db, id, &execution_workspace).await?;
        task = task_store::load_task(db, id).await?.unwrap_or(task);

        if let Err(message) =
            policy_engine::validate_execution_workspace(&task, &execution_workspace)
        {
            task_store::update_status(
                db,
                id,
                TaskStatus::NeedsInput,
                EventKind::Error,
                message.clone(),
            )
            .await?;
            return Err(message);
        }

        if task.runner_session_id.is_none() {
            if let Some(session_id) = runner_adapters::initial_runner_session_id(&task) {
                task_store::set_runner_session_id(db, id, &session_id).await?;
                task_store::insert_event(
                    db,
                    id,
                    EventKind::Lifecycle,
                    format!("Runner session id: {session_id}"),
                )
                .await?;
                task = task_store::load_task(db, id).await?.unwrap_or(task);
            }
        }

        let mut runner = runner_adapters::build_runner_command(&task, &execution_workspace)
            .map_err(|message| {
                format!("Cannot start {} runner: {message}", task.runner.as_str())
            })?;

        task_store::update_status(
            db,
            id,
            TaskStatus::Running,
            EventKind::Lifecycle,
            format!("Starting {} runner in {execution_workspace}", runner.label),
        )
        .await?;

        runner.command.stdout(std::process::Stdio::piped());
        runner.command.stderr(std::process::Stdio::piped());
        runner.command.stdin(if runner.keep_stdin {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        });

        let mut child = runner
            .command
            .spawn()
            .map_err(|error| format!("failed to start command: {error}"))?;

        if let Some(stdout) = child.stdout.take() {
            spawn_output_reader(
                db.clone(),
                id,
                task.runner.clone(),
                task.execution_policy().clone(),
                stdout,
                EventKind::Stdout,
            );
        }

        if let Some(stderr) = child.stderr.take() {
            spawn_output_reader(
                db.clone(),
                id,
                task.runner.clone(),
                task.execution_policy().clone(),
                stderr,
                EventKind::Stderr,
            );
        }

        let stdin = runner.keep_stdin.then(|| child.stdin.take()).flatten();
        self.children
            .lock()
            .await
            .insert(id, RunningProcess { child, stdin });
        spawn_task_monitor(
            self.children.clone(),
            db.clone(),
            id,
            task.execution_policy()
                .max_runtime_minutes
                .unwrap_or(task.budget_minutes),
        );

        Ok(())
    }

    pub async fn stop(&self, db: &SqlitePool, id: Uuid) -> Result<(), String> {
        let mut process = self.children.lock().await.remove(&id);

        if let Some(process) = process.as_mut() {
            process
                .child
                .kill()
                .await
                .map_err(|error| format!("failed to stop command: {error}"))?;
        }

        worktree_review::capture_task_diff(db, id).await?;

        task_store::update_status(
            db,
            id,
            TaskStatus::Stopped,
            EventKind::Lifecycle,
            "Task stopped by user".to_string(),
        )
        .await
    }

    pub async fn reply(&self, db: &SqlitePool, id: Uuid, message: &str) -> Result<(), String> {
        let task = task_store::load_task(db, id)
            .await?
            .ok_or_else(|| "task not found".to_string())?;
        let mut children = self.children.lock().await;
        if let Some(process) = children.get_mut(&id) {
            if let Some(stdin) = process.stdin.as_mut() {
                let reply = format!("{message}\n");
                if let Err(error) = stdin.write_all(reply.as_bytes()).await {
                    drop(children);
                    let error = format!("failed to send reply: {error}");
                    task_store::insert_event(
                        db,
                        id,
                        EventKind::Error,
                        policy_engine::redact_secrets(&error, task.execution_policy()),
                    )
                    .await?;
                    return Err(error);
                }
                if let Err(error) = stdin.flush().await {
                    drop(children);
                    let error = format!("failed to flush reply: {error}");
                    task_store::insert_event(
                        db,
                        id,
                        EventKind::Error,
                        policy_engine::redact_secrets(&error, task.execution_policy()),
                    )
                    .await?;
                    return Err(error);
                }
                drop(children);
                task_store::insert_event(
                    db,
                    id,
                    EventKind::Input,
                    policy_engine::redact_secrets(
                        &format!("User reply: {message}"),
                        task.execution_policy(),
                    ),
                )
                .await?;
                return Ok(());
            }

            drop(children);
            let error = live_runner_reply_unavailable_message();
            task_store::insert_event(db, id, EventKind::Error, error.to_string()).await?;
            return Err(error.to_string());
        }
        drop(children);

        let Some(mut command) = (match runner_adapters::build_session_reply_command(&task, message)
        {
            Ok(command) => command,
            Err(error) => {
                task_store::insert_event(
                    db,
                    id,
                    EventKind::Error,
                    policy_engine::redact_secrets(&error, task.execution_policy()),
                )
                .await?;
                return Err(error);
            }
        }) else {
            let error = "runner session reply is not available for this task".to_string();
            task_store::insert_event(db, id, EventKind::Error, error.clone()).await?;
            return Err(error);
        };

        command.command.stdout(std::process::Stdio::piped());
        command.command.stderr(std::process::Stdio::piped());
        command.command.stdin(std::process::Stdio::null());

        let child = command
            .command
            .spawn()
            .map_err(|error| format!("failed to start session reply: {error}"));
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                task_store::insert_event(
                    db,
                    id,
                    EventKind::Error,
                    policy_engine::redact_secrets(&error, task.execution_policy()),
                )
                .await?;
                return Err(error);
            }
        };

        if let Some(stdout) = child.stdout.take() {
            spawn_output_reader(
                db.clone(),
                id,
                task.runner.clone(),
                task.execution_policy().clone(),
                stdout,
                EventKind::Stdout,
            );
        }

        if let Some(stderr) = child.stderr.take() {
            spawn_output_reader(
                db.clone(),
                id,
                task.runner.clone(),
                task.execution_policy().clone(),
                stderr,
                EventKind::Stderr,
            );
        }

        self.children
            .lock()
            .await
            .insert(id, RunningProcess { child, stdin: None });
        spawn_task_monitor(
            self.children.clone(),
            db.clone(),
            id,
            task.execution_policy()
                .max_runtime_minutes
                .unwrap_or(task.budget_minutes),
        );

        task_store::insert_event(
            db,
            id,
            EventKind::Input,
            policy_engine::redact_secrets(
                &format!("User reply: {message}"),
                task.execution_policy(),
            ),
        )
        .await?;
        task_store::insert_event(
            db,
            id,
            EventKind::Lifecycle,
            format!("Resuming runner session with {}", command.display),
        )
        .await?;
        task_store::update_status(
            db,
            id,
            TaskStatus::Running,
            EventKind::Lifecycle,
            "Runner session reply started".to_string(),
        )
        .await?;

        Ok(())
    }
}

fn live_runner_reply_unavailable_message() -> &'static str {
    "Runner is still running and cannot accept inline replies through stdin"
}

fn spawn_output_reader<R>(
    db: SqlitePool,
    task_id: Uuid,
    runner: crate::models::RunnerKind,
    policy: crate::models::TaskPolicy,
    reader: R,
    fallback_kind: EventKind,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let parsed =
                runner_adapters::parse_runner_output(&runner, fallback_kind.clone(), &line);
            let message = policy_engine::redact_secrets(&parsed.message, &policy);

            if let Err(error) = task_store::insert_event(&db, task_id, parsed.kind, message).await {
                eprintln!("failed to persist task output: {error}");
            }

            if parsed.needs_input {
                let _ = task_store::update_status(
                    &db,
                    task_id,
                    TaskStatus::NeedsInput,
                    EventKind::Lifecycle,
                    "Runner emitted a structured needs-input signal".to_string(),
                )
                .await;
            }

            if let Some(session_id) = parsed.session_id {
                let _ = task_store::set_runner_session_id(&db, task_id, &session_id).await;
                let _ = task_store::insert_event(
                    &db,
                    task_id,
                    EventKind::Lifecycle,
                    format!("Runner session id: {session_id}"),
                )
                .await;
            }

            let _ = task_store::add_cost_delta(&db, task_id, parsed.cost_delta).await;
        }
    });
}

fn spawn_task_monitor(
    children: Arc<Mutex<HashMap<Uuid, RunningProcess>>>,
    db: SqlitePool,
    task_id: Uuid,
    budget_minutes: u32,
) {
    tokio::spawn(async move {
        let mut elapsed = Duration::ZERO;
        let tick = Duration::from_millis(500);
        let budget = Duration::from_secs(u64::from(budget_minutes) * 60);

        loop {
            tokio::time::sleep(tick).await;
            elapsed += tick;

            if elapsed >= budget {
                if let Some(mut process) = children.lock().await.remove(&task_id) {
                    let _ = process.child.kill().await;
                    let _ = worktree_review::capture_task_diff(&db, task_id).await;
                    let _ = task_store::add_cost_delta(
                        &db,
                        task_id,
                        CostLedger {
                            runtime_millis: elapsed.as_millis() as u64,
                            ..CostLedger::default()
                        },
                    )
                    .await;
                    let _ = task_store::update_status(
                        &db,
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
                let mut children = children.lock().await;
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
                    let has_changes = worktree_review::capture_task_diff(&db, task_id)
                        .await
                        .unwrap_or(false);
                    let _ = task_store::add_cost_delta(
                        &db,
                        task_id,
                        CostLedger {
                            runtime_millis: elapsed.as_millis() as u64,
                            ..CostLedger::default()
                        },
                    )
                    .await;
                    let _ = task_store::update_status(
                        &db,
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
                    let _ = worktree_review::capture_task_diff(&db, task_id).await;
                    let _ = task_store::add_cost_delta(
                        &db,
                        task_id,
                        CostLedger {
                            runtime_millis: elapsed.as_millis() as u64,
                            ..CostLedger::default()
                        },
                    )
                    .await;
                    let _ = task_store::update_status(
                        &db,
                        task_id,
                        TaskStatus::Failed,
                        EventKind::Lifecycle,
                        format!("Task failed with status {exit_status}"),
                    )
                    .await;
                    return;
                }
                Some(Err(error)) => {
                    let _ = worktree_review::capture_task_diff(&db, task_id).await;
                    let _ = task_store::update_status(
                        &db,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_runner_without_stdin_has_clear_reply_error() {
        assert_eq!(
            live_runner_reply_unavailable_message(),
            "Runner is still running and cannot accept inline replies through stdin"
        );
    }
}
