use crate::{
    models::{EventKind, TaskDiffResponse, TaskStatus, WorktreeActionResponse},
    runner_adapters::find_executable,
    task_store,
};
use sqlx::SqlitePool;
use std::path::{Path as FsPath, PathBuf};
use tokio::process::Command;
use uuid::Uuid;

pub async fn get_task_diff(db: &SqlitePool, id: Uuid) -> Result<TaskDiffResponse, String> {
    let task = task_store::load_task(db, id)
        .await?
        .ok_or_else(|| "task not found".to_string())?;

    let Some(worktree_path) = task.worktree_path.clone() else {
        return Ok(TaskDiffResponse {
            task_id: id,
            isolated: false,
            worktree_path: None,
            base_commit: None,
            stat: task.diff_stat.unwrap_or_default(),
            patch: String::new(),
        });
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

    Ok(TaskDiffResponse {
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
    })
}

pub async fn prepare_execution_workspace(db: &SqlitePool, task_id: Uuid) -> Result<String, String> {
    let task = task_store::load_task(db, task_id)
        .await?
        .ok_or_else(|| "task not found".to_string())?;

    if let Some(worktree_path) = &task.worktree_path {
        if FsPath::new(worktree_path).exists() {
            return Ok(worktree_path.clone());
        }
    }

    if find_executable("git").is_none() {
        task_store::insert_event(
            db,
            task.id,
            EventKind::Lifecycle,
            "Git was not found; running without worktree isolation".to_string(),
        )
        .await?;
        return Ok(task.workspace);
    }

    let Ok(root) = git_output_lossy(&task.workspace, &["rev-parse", "--show-toplevel"]).await
    else {
        task_store::insert_event(
            db,
            task.id,
            EventKind::Lifecycle,
            "Workspace is not a git repository; running without worktree isolation".to_string(),
        )
        .await?;
        return Ok(task.workspace);
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
    task_store::set_worktree(
        db,
        task.id,
        &worktree_path,
        (!base_commit.is_empty()).then_some(base_commit),
    )
    .await?;

    task_store::insert_event(
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

pub async fn capture_task_diff(db: &SqlitePool, task_id: Uuid) -> Result<bool, String> {
    let Some(task) = task_store::load_task(db, task_id).await? else {
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
    let summary = diff_summary(&status, &stat);

    task_store::set_diff_stat(
        db,
        task_id,
        (!summary.is_empty()).then_some(summary.clone()),
    )
    .await?;

    if summary.is_empty() {
        task_store::insert_event(
            db,
            task_id,
            EventKind::Diff,
            "No file changes detected in isolated worktree".to_string(),
        )
        .await?;
    } else {
        task_store::insert_event(db, task_id, EventKind::Diff, summary).await?;
    }

    Ok(!status.trim().is_empty() || !stat.trim().is_empty())
}

pub async fn merge_task_worktree(
    db: &SqlitePool,
    id: Uuid,
) -> Result<WorktreeActionResponse, String> {
    let task = task_store::load_task(db, id)
        .await?
        .ok_or_else(|| "task not found".to_string())?;
    let Some(worktree_path) = task.worktree_path.clone() else {
        return Err("task has no worktree".to_string());
    };

    if task.worktree_merged_at.is_some() {
        return Err("worktree has already been merged".to_string());
    }

    capture_task_diff(db, id).await?;
    let source_status = git_output_lossy(&worktree_path, &["status", "--short"]).await?;
    if source_status.trim().is_empty() {
        return Err("worktree has no changes to merge".to_string());
    }

    let repo_root = git_output_lossy(&task.workspace, &["rev-parse", "--show-toplevel"])
        .await?
        .trim()
        .to_string();
    let source_root = PathBuf::from(&worktree_path);

    if let Err(error) = git_output_lossy(
        &repo_root,
        &["diff", "--quiet", "--ignore-submodules", "--"],
    )
    .await
    {
        return Err(format!(
            "target workspace has uncommitted tracked changes: {error}"
        ));
    }

    let patch = git_output_lossy(&worktree_path, &["diff", "--binary", "HEAD", "--"]).await?;
    apply_patch_to_target(&repo_root, &patch).await?;

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
        copy_worktree_file(&source_root, FsPath::new(&repo_root), file).await?;
    }

    task_store::mark_worktree_merged(db, id).await?;
    task_store::insert_event(
        db,
        id,
        EventKind::Lifecycle,
        "Merged worktree changes into target workspace".to_string(),
    )
    .await?;

    let task = task_store::load_task(db, id)
        .await?
        .ok_or_else(|| "task disappeared after merge".to_string())?;

    Ok(WorktreeActionResponse {
        task,
        message: "Merged worktree changes into target workspace".to_string(),
    })
}

pub async fn cleanup_task_worktree(
    db: &SqlitePool,
    id: Uuid,
) -> Result<WorktreeActionResponse, String> {
    let task = task_store::load_task(db, id)
        .await?
        .ok_or_else(|| "task not found".to_string())?;
    let Some(worktree_path) = task.worktree_path.clone() else {
        return Err("task has no worktree".to_string());
    };

    if task.status == TaskStatus::Running {
        return Err("cannot clean up a running task".to_string());
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
            .map_err(|error| error.to_string())?;
    }

    task_store::mark_worktree_cleaned(db, id).await?;
    task_store::insert_event(
        db,
        id,
        EventKind::Lifecycle,
        "Cleaned up isolated worktree".to_string(),
    )
    .await?;

    let task = task_store::load_task(db, id)
        .await?
        .ok_or_else(|| "task disappeared after cleanup".to_string())?;

    Ok(WorktreeActionResponse {
        task,
        message: "Cleaned up isolated worktree".to_string(),
    })
}

pub async fn git_output_lossy(cwd: &str, args: &[&str]) -> Result<String, String> {
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

fn diff_summary(status: &str, stat: &str) -> String {
    [status.trim(), stat.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_summary_omits_empty_parts() {
        assert_eq!(diff_summary("M src/main.rs", ""), "M src/main.rs");
        assert_eq!(diff_summary("M a", "a | 1 +"), "M a\n\na | 1 +");
    }
}
