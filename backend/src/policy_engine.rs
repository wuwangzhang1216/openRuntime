use crate::models::{NetworkMode, RunnerKind, Task, TaskPolicy};
use std::path::{Path, PathBuf};

pub fn validate_task(task: &Task) -> Result<(), String> {
    validate_task_plan(
        &task.runner,
        &task.command,
        &task.prompt,
        &task.workspace,
        task.execution_policy(),
        task.approved_at.map(|_| ()),
    )
}

pub fn validate_task_plan(
    runner: &RunnerKind,
    command: &str,
    prompt: &str,
    workspace: &str,
    policy: &TaskPolicy,
    approved: Option<()>,
) -> Result<(), String> {
    validate_workspace_allowlist(workspace, policy)?;

    if policy.require_approval && approved.is_none() {
        return Err("Policy requires manual approval before this task can run".to_string());
    }

    let inspected = match runner {
        RunnerKind::Shell => command.to_lowercase(),
        RunnerKind::ClaudeCode | RunnerKind::Codex => prompt.to_lowercase(),
    };

    for blocked in &policy.blocked_commands {
        if !blocked.is_empty() && inspected.contains(&blocked.to_lowercase()) {
            return Err(format!("Command blocked by policy: {blocked}"));
        }
    }

    if contains_network_command(&inspected) {
        match effective_network_mode(policy) {
            NetworkMode::Disabled => {
                return Err("Network access is disabled for this task".to_string());
            }
            NetworkMode::Localhost if !network_command_targets_localhost(&inspected) => {
                return Err("Network access is limited to localhost for this task".to_string());
            }
            NetworkMode::Localhost | NetworkMode::Enabled => {}
        }
    }

    if !policy.allow_git_write
        && contains_any(
            &inspected,
            &[
                "git push",
                "git commit",
                "git tag",
                "git merge",
                "git rebase",
                "git checkout",
                "git reset",
                "git stash",
            ],
        )
    {
        return Err("Git write operations are disabled for this task".to_string());
    }

    if !policy.allow_secrets
        && contains_any(
            &inspected,
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

pub fn effective_network_mode(policy: &TaskPolicy) -> NetworkMode {
    if policy.allow_network {
        NetworkMode::Enabled
    } else {
        policy.network_mode.clone()
    }
}

pub fn execution_env(policy: &TaskPolicy) -> Vec<(&'static str, String)> {
    let mut env = vec![
        (
            "OPENRUNTIME_NETWORK_MODE",
            match effective_network_mode(policy) {
                NetworkMode::Disabled => "disabled",
                NetworkMode::Localhost => "localhost",
                NetworkMode::Enabled => "enabled",
            }
            .to_string(),
        ),
        (
            "OPENRUNTIME_ALLOWED_MCP_TOOLS",
            policy.allowed_mcp_tools.join(","),
        ),
        (
            "OPENRUNTIME_ALLOWED_FILE_GLOBS",
            policy.allowed_file_globs.join(","),
        ),
        (
            "OPENRUNTIME_ALLOWED_WORKSPACES",
            policy.allowed_workspaces.join(","),
        ),
        ("OPENRUNTIME_GIT_WRITE", policy.allow_git_write.to_string()),
    ];

    if let Some(budget_cents) = policy.budget_cents {
        env.push(("OPENRUNTIME_BUDGET_CENTS", budget_cents.to_string()));
    }

    if let Some(max_runtime_minutes) = policy.max_runtime_minutes {
        env.push((
            "OPENRUNTIME_MAX_RUNTIME_MINUTES",
            max_runtime_minutes.to_string(),
        ));
    }

    if !policy.allow_secrets {
        env.push(("OPENRUNTIME_SECRET_ACCESS", "disabled".to_string()));
    }

    env
}

pub fn validate_execution_workspace(task: &Task, execution_workspace: &str) -> Result<(), String> {
    let policy = task.execution_policy();
    validate_workspace_allowlist(&task.workspace, policy)?;

    let execution_workspace = canonical_or_raw(execution_workspace);
    let allowed_root = task
        .worktree_path
        .as_deref()
        .map(canonical_or_raw)
        .unwrap_or_else(|| canonical_or_raw(&task.workspace));

    if path_starts_with(&execution_workspace, &allowed_root) {
        Ok(())
    } else {
        Err("Execution workspace is outside the task boundary".to_string())
    }
}

pub fn redact_secrets(message: &str, policy: &TaskPolicy) -> String {
    if policy.allow_secrets {
        return message.to_string();
    }

    redact_tokenish_words(message)
}

fn validate_workspace_allowlist(workspace: &str, policy: &TaskPolicy) -> Result<(), String> {
    if policy.allowed_workspaces.is_empty() {
        return Ok(());
    }

    let workspace = canonical_or_raw(workspace);
    let allowed = policy
        .allowed_workspaces
        .iter()
        .map(|path| canonical_or_raw(path))
        .any(|allowed| workspace.starts_with(&allowed));

    if allowed {
        Ok(())
    } else {
        Err("Workspace is outside this task's allowlist".to_string())
    }
}

fn canonical_or_raw(path: &str) -> PathBuf {
    Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path))
}

fn path_starts_with(path: &Path, parent: &Path) -> bool {
    path == parent || path.starts_with(parent)
}

fn contains_any(command: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| command.contains(needle))
}

fn contains_network_command(command: &str) -> bool {
    contains_any(command, &["curl", "wget", "ssh", "scp", "nc "])
}

fn network_command_targets_localhost(command: &str) -> bool {
    let mut has_local_target = false;

    for token in command.split_whitespace() {
        let token = token
            .trim_matches(|ch: char| matches!(ch, '"' | '\'' | ',' | ';' | '(' | ')' | '[' | ']'));
        if token_is_local_target(token) {
            has_local_target = true;
        } else if token_looks_like_remote_target(token) {
            return false;
        }
    }

    has_local_target
}

fn token_is_local_target(token: &str) -> bool {
    let target = network_target(token);
    target == "localhost"
        || target == "127.0.0.1"
        || target == "::1"
        || target == "[::1]"
        || target.starts_with("127.")
}

fn token_looks_like_remote_target(token: &str) -> bool {
    let target = network_target(token);
    let looks_like_url = token.starts_with("http://") || token.starts_with("https://");
    let looks_like_host = target.contains('.') || token.contains('@') || token.contains(':');

    (looks_like_url || looks_like_host) && !token_is_local_target(token)
}

fn network_target(token: &str) -> String {
    let without_scheme = token
        .strip_prefix("http://")
        .or_else(|| token.strip_prefix("https://"))
        .unwrap_or(token);
    let without_user = without_scheme.rsplit('@').next().unwrap_or(without_scheme);
    let without_path = without_user.split('/').next().unwrap_or(without_user);
    let without_port = if without_path == "::1" {
        "::1"
    } else if without_path.starts_with("[::1]") {
        "[::1]"
    } else {
        without_path.split(':').next().unwrap_or(without_path)
    };

    without_port
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '.' && ch != ':')
        .to_string()
}

fn redact_tokenish_words(message: &str) -> String {
    let mut redact_next = false;
    let mut saw_api = false;
    message
        .split_whitespace()
        .map(|word| {
            let lower = word.to_lowercase();
            let marker = lower.trim_end_matches(':');
            let redacted = if redact_next || is_secret_assignment(&lower) || is_tokenish_word(word)
            {
                redact_next = false;
                saw_api = false;
                redact_word(word)
            } else {
                word.to_string()
            };

            if saw_api && marker == "key" {
                redact_next = true;
                saw_api = false;
            } else if marker == "api" {
                saw_api = true;
            } else if matches!(marker, "bearer" | "authorization") {
                redact_next = true;
                saw_api = false;
            } else {
                saw_api = false;
            }

            redacted
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_secret_assignment(lower: &str) -> bool {
    [
        "api_key=",
        "apikey=",
        "api-key=",
        "access_token=",
        "secret=",
        "password=",
        "token=",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
}

fn is_tokenish_word(word: &str) -> bool {
    let trimmed = word.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-');
    let looks_like_key = trimmed.len() >= 24
        && trimmed.chars().any(|ch| ch.is_ascii_digit())
        && trimmed.chars().any(|ch| ch.is_ascii_uppercase())
        && trimmed.chars().any(|ch| ch.is_ascii_lowercase());
    let looks_like_openai_key = trimmed.starts_with("sk-") && trimmed.len() > 20;

    looks_like_key || looks_like_openai_key
}

fn redact_word(word: &str) -> String {
    if let Some((name, _)) = word.split_once('=') {
        format!("{name}=[redacted]")
    } else {
        "[redacted]".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TaskPolicy;

    #[test]
    fn blocks_network_when_network_mode_is_disabled() {
        let policy = TaskPolicy {
            blocked_commands: Vec::new(),
            ..TaskPolicy::default()
        };

        let result = validate_task_plan(
            &RunnerKind::Shell,
            "curl https://example.com",
            "",
            ".",
            &policy,
            Some(()),
        );

        assert!(result.unwrap_err().contains("Network access"));
    }

    #[test]
    fn allow_network_compatibility_flag_enables_network() {
        let policy = TaskPolicy {
            allow_network: true,
            blocked_commands: Vec::new(),
            ..TaskPolicy::default()
        };

        let result = validate_task_plan(
            &RunnerKind::Shell,
            "curl https://example.com",
            "",
            ".",
            &policy,
            Some(()),
        );

        assert!(result.is_ok());
    }

    #[test]
    fn localhost_network_mode_blocks_external_targets() {
        let policy = TaskPolicy {
            network_mode: NetworkMode::Localhost,
            blocked_commands: Vec::new(),
            ..TaskPolicy::default()
        };

        let result = validate_task_plan(
            &RunnerKind::Shell,
            "curl https://example.com",
            "",
            ".",
            &policy,
            Some(()),
        );

        assert!(result.unwrap_err().contains("localhost"));
    }

    #[test]
    fn localhost_network_mode_allows_local_targets() {
        let policy = TaskPolicy {
            network_mode: NetworkMode::Localhost,
            blocked_commands: Vec::new(),
            ..TaskPolicy::default()
        };

        let result = validate_task_plan(
            &RunnerKind::Shell,
            "curl http://localhost:3000/health",
            "",
            ".",
            &policy,
            Some(()),
        );

        assert!(result.is_ok());
    }

    #[test]
    fn redacts_secret_like_output() {
        let policy = TaskPolicy::default();

        assert_eq!(
            redact_secrets("api_key=sk-test-value", &policy),
            "api_key=[redacted]"
        );
        assert_eq!(
            redact_secrets("Authorization: Bearer sk-test-value-1234567890", &policy),
            "Authorization: [redacted] [redacted]"
        );
        assert_eq!(
            redact_secrets("api key: sk-test-value-1234567890", &policy),
            "api key: [redacted]"
        );
    }

    #[test]
    fn exposes_policy_boundary_as_runner_environment() {
        let policy = TaskPolicy {
            allowed_workspaces: vec!["/repo".to_string()],
            allowed_file_globs: vec!["src/**".to_string()],
            allowed_mcp_tools: vec!["github.search".to_string()],
            network_mode: NetworkMode::Localhost,
            budget_cents: Some(25),
            max_runtime_minutes: Some(5),
            ..TaskPolicy::default()
        };
        let env = execution_env(&policy)
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(
            env.get("OPENRUNTIME_ALLOWED_WORKSPACES")
                .map(String::as_str),
            Some("/repo")
        );
        assert_eq!(
            env.get("OPENRUNTIME_ALLOWED_FILE_GLOBS")
                .map(String::as_str),
            Some("src/**")
        );
        assert_eq!(
            env.get("OPENRUNTIME_ALLOWED_MCP_TOOLS").map(String::as_str),
            Some("github.search")
        );
        assert_eq!(
            env.get("OPENRUNTIME_NETWORK_MODE").map(String::as_str),
            Some("localhost")
        );
        assert_eq!(
            env.get("OPENRUNTIME_BUDGET_CENTS").map(String::as_str),
            Some("25")
        );
    }

    #[test]
    fn enforces_workspace_allowlist() {
        let policy = TaskPolicy {
            allowed_workspaces: vec!["/tmp/project".to_string()],
            ..TaskPolicy::default()
        };

        let result = validate_task_plan(
            &RunnerKind::Shell,
            "echo ok",
            "",
            "/var/tmp/project",
            &policy,
            Some(()),
        );

        assert!(result.unwrap_err().contains("allowlist"));
    }

    #[test]
    fn validates_execution_workspace_inside_worktree_boundary() {
        let task = Task {
            id: uuid::Uuid::nil(),
            title: "test".to_string(),
            prompt: "prompt".to_string(),
            runner: RunnerKind::Shell,
            command: "echo ok".to_string(),
            workspace: "/repo/frontend".to_string(),
            worktree_path: Some("/repo/.openruntime/worktrees/task".to_string()),
            execution_workspace: Some("/repo/.openruntime/worktrees/task/frontend".to_string()),
            runner_session_id: None,
            base_commit: None,
            diff_stat: None,
            approved_at: Some(chrono::Utc::now()),
            worktree_merged_at: None,
            worktree_cleaned_at: None,
            status: crate::models::TaskStatus::Queued,
            budget_minutes: 1,
            policy: TaskPolicy::default(),
            effective_policy: None,
            cost_ledger: crate::models::CostLedger::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            events: Vec::new(),
        };

        assert!(
            validate_execution_workspace(&task, "/repo/.openruntime/worktrees/task/frontend")
                .is_ok()
        );
        assert!(
            validate_execution_workspace(&task, "/tmp/outside")
                .unwrap_err()
                .contains("outside")
        );
    }
}
