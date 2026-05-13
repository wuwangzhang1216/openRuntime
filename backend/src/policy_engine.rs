use crate::models::{NetworkMode, RunnerKind, Task, TaskPolicy};
use std::path::{Path, PathBuf};

pub fn validate_task(task: &Task) -> Result<(), String> {
    validate_task_plan(
        &task.runner,
        &task.command,
        &task.prompt,
        &task.workspace,
        &task.policy,
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

    if effective_network_mode(policy) == NetworkMode::Disabled
        && contains_any(&inspected, &["curl", "wget", "ssh", "scp", "nc "])
    {
        return Err("Network access is disabled for this task".to_string());
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
    ];

    if !policy.allow_secrets {
        env.push(("OPENRUNTIME_SECRET_ACCESS", "disabled".to_string()));
    }

    env
}

pub fn redact_secrets(message: &str, policy: &TaskPolicy) -> String {
    if policy.allow_secrets {
        return message.to_string();
    }

    let lower = message.to_lowercase();
    if contains_any(
        &lower,
        &[
            "api_key=",
            "api key:",
            "access_token=",
            "bearer ",
            "secret=",
            "password=",
            "keychain",
            ".env",
        ],
    ) {
        return "[redacted: secret-like output]".to_string();
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

fn contains_any(command: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| command.contains(needle))
}

fn redact_tokenish_words(message: &str) -> String {
    message
        .split_whitespace()
        .map(|word| {
            let looks_like_key = word.len() >= 24
                && word.chars().any(|ch| ch.is_ascii_digit())
                && word.chars().any(|ch| ch.is_ascii_uppercase())
                && word.chars().any(|ch| ch.is_ascii_lowercase());
            let looks_like_openai_key = word.starts_with("sk-") && word.len() > 20;

            if looks_like_key || looks_like_openai_key {
                "[redacted]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    fn redacts_secret_like_output() {
        let policy = TaskPolicy::default();

        assert_eq!(
            redact_secrets("api_key=sk-test-value", &policy),
            "[redacted: secret-like output]"
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
}
