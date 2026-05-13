# openRuntime

Local-first agent management system.

## Stack

- `frontend/`: Next.js, React, Tailwind CSS
- `backend/`: Rust, Axum, Tokio

## Run

Start the Rust API:

```bash
cd backend
cargo run
```

Start the Next.js app:

```bash
cd frontend
npm run dev
```

Open `http://localhost:3000`. The frontend expects the backend at
`http://127.0.0.1:8080` unless `NEXT_PUBLIC_API_URL` is set.

## Current v0

- Create local managed tasks.
- Run shell, Codex, and Claude Code style tasks through the Rust supervisor.
- Detect runner availability with `/runners`.
- Persist tasks and event history in SQLite.
- Poll task state from the Next.js control plane.
- Stop running tasks.
- Capture lifecycle, stdout, stderr, and error events.
- Enforce a per-task runtime budget.
- Enforce a first-pass task policy for network, git write, secrets, approval
  gates, and blocked command fragments.

SQLite defaults to `data/openruntime.sqlite3`. Override it with
`OPENRUNTIME_DB=/absolute/path/to/file.sqlite3`. The legacy
`MANAGED_AGENTS_DB` variable is still accepted for existing installs.

Runner support:

- `shell`: runs `/bin/sh -lc <command>`.
- `codex`: runs `codex exec --json --skip-git-repo-check -s workspace-write -C <workspace> <goal>`.
- `claude-code`: runs `claude -p <goal>` when the Claude Code CLI is installed
  and available in `PATH`.

## Next targets

- Add git worktree isolation.
- Add Codex and Claude Code adapters.
- Add richer policy scopes for filesystem paths and MCP/tool access.
- Add diff-first review surfaces.
