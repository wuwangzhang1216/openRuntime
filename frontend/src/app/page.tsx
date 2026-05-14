"use client";

import {
  Activity,
  ArrowUp,
  Ban,
  Check,
  ChevronDown,
  CircleStop,
  Clock,
  Folder,
  FolderPlus,
  Gauge,
  GitBranch,
  Hand,
  KeyRound,
  Loader2,
  Pin,
  Play,
  RefreshCw,
  Search,
  Shield,
  ShieldCheck,
  Terminal,
  Wifi,
} from "lucide-react";
import {
  FormEvent,
  ReactNode,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

const API_URL = process.env.NEXT_PUBLIC_API_URL ?? "http://127.0.0.1:8080";
const DEFAULT_WORKSPACE = "/Users/wangzhangwu/openRuntime";

type RunnerKind = "shell" | "claude-code" | "codex";
type TaskStatus =
  | "queued"
  | "running"
  | "needs-input"
  | "ready-for-review"
  | "completed"
  | "failed"
  | "stopped";
type EventKind = "lifecycle" | "stdout" | "stderr" | "diff" | "input" | "error";

type EventMetadata = {
  category?: string;
  reason?: string;
  source?: string;
  attempt_id?: string;
  attempt_number?: number;
  [key: string]: unknown;
};

type TaskEvent = {
  id: string;
  task_id: string;
  attempt_id: string | null;
  kind: EventKind;
  message: string;
  metadata: EventMetadata | null;
  created_at: string;
};

type TaskPolicy = {
  allow_network: boolean;
  allow_git_write: boolean;
  allow_secrets: boolean;
  require_approval: boolean;
  blocked_commands: string[];
  allowed_workspaces?: string[];
  allowed_file_globs?: string[];
  allowed_mcp_tools?: string[];
  network_mode?: "disabled" | "localhost" | "enabled";
  budget_cents?: number | null;
  max_runtime_minutes?: number | null;
};

type CostLedger = {
  runtime_millis: number;
  input_tokens: number;
  output_tokens: number;
  tool_calls: number;
  estimated_cents: number;
};

type RunnerInfo = {
  runner: RunnerKind;
  available: boolean;
  command: string;
};

type TaskAttempt = {
  id: string;
  task_id: string;
  attempt_number: number;
  runner: RunnerKind;
  status: TaskStatus;
  execution_workspace: string | null;
  runner_session_id: string | null;
  started_at: string;
  finished_at: string | null;
  exit_status: string | null;
  summary: string | null;
};

type Task = {
  id: string;
  title: string;
  prompt: string;
  runner: RunnerKind;
  command: string;
  workspace: string;
  worktree_path: string | null;
  execution_workspace: string | null;
  runner_session_id: string | null;
  base_commit: string | null;
  diff_stat: string | null;
  approved_at: string | null;
  worktree_merged_at: string | null;
  worktree_cleaned_at: string | null;
  status: TaskStatus;
  budget_minutes: number;
  policy: TaskPolicy;
  effective_policy: TaskPolicy | null;
  cost_ledger: CostLedger;
  created_at: string;
  updated_at: string;
  events: TaskEvent[];
  attempts: TaskAttempt[];
  current_attempt: TaskAttempt | null;
};

type FormState = {
  title: string;
  prompt: string;
  runner: RunnerKind;
  command: string;
  workspace: string;
  budgetMinutes: number;
  allowNetwork: boolean;
  allowGitWrite: boolean;
  allowSecrets: boolean;
  requireApproval: boolean;
  blockedCommands: string;
};

type PermissionPreset = "default" | "review" | "full";

type WorkspaceProject = {
  name: string;
  path: string;
};

type TaskDiff = {
  task_id: string;
  isolated: boolean;
  worktree_path: string | null;
  runner_session_id: string | null;
  base_commit: string | null;
  stat: string;
  patch: string;
};

const initialForm: FormState = {
  title: "",
  prompt: "Inspect the workspace and summarize what this project currently does.",
  runner: "codex",
  command: "printf 'planning\\n'; sleep 2; printf 'ready for review\\n'",
  workspace: DEFAULT_WORKSPACE,
  budgetMinutes: 15,
  allowNetwork: false,
  allowGitWrite: false,
  allowSecrets: false,
  requireApproval: false,
  blockedCommands: "rm -rf, sudo, git push, curl, wget, ssh, scp",
};

const DEFAULT_BLOCKED_COMMANDS = initialForm.blockedCommands;

const statusStyles: Record<TaskStatus, string> = {
  queued: "border-slate-200 bg-slate-50 text-slate-600",
  running: "border-slate-200 bg-slate-50 text-slate-800",
  "needs-input": "border-amber-200 bg-[#fff8e7] text-amber-800",
  "ready-for-review": "border-slate-200 bg-white text-slate-800",
  completed: "border-emerald-200 bg-emerald-50 text-emerald-800",
  failed: "border-rose-200 bg-rose-50 text-rose-800",
  stopped: "border-slate-200 bg-white text-slate-600",
};

const eventStyles: Record<EventKind, string> = {
  lifecycle: "border-slate-200 text-slate-500",
  stdout: "border-emerald-100 text-emerald-700",
  stderr: "border-amber-200 text-amber-700",
  diff: "border-sky-200 text-sky-700",
  input: "border-indigo-200 text-indigo-700",
  error: "border-rose-200 text-rose-700",
};

const runnerLabels: Record<RunnerKind, string> = {
  codex: "Codex",
  "claude-code": "Claude Code",
  shell: "Shell",
};

const terminalStatuses = new Set<TaskStatus>([
  "completed",
  "failed",
  "needs-input",
  "ready-for-review",
  "stopped",
]);

type ConfirmableTaskAction = "stop" | "approve" | "merge" | "cleanup";

export default function Home() {
  const [tasks, setTasks] = useState<Task[]>([]);
  const [runners, setRunners] = useState<RunnerInfo[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [form, setForm] = useState<FormState>(initialForm);
  const [isSubmitting, setIsSubmitting] = useState(false);
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const errorSource = useRef<"action" | "backend" | null>(null);
  const refreshSequence = useRef(0);
  const refreshAbort = useRef<AbortController | null>(null);
  const refreshInFlight = useRef(false);

  const selectedTask = useMemo(
    () =>
      selectedId
        ? tasks.find((task) => task.id === selectedId)
        : tasks[0],
    [selectedId, tasks],
  );

  const stats = useMemo(() => {
    const running = tasks.filter((task) => task.status === "running").length;
    const blocked = tasks.filter(
      (task) => task.status === "needs-input" || task.status === "failed",
    ).length;
    const done = tasks.filter((task) => task.status === "completed").length;
    return { total: tasks.length, running, blocked, done };
  }, [tasks]);

  const refreshTasks = useCallback(async (showSpinner = true) => {
    if (refreshInFlight.current && !showSpinner) {
      return;
    }

    const requestId = refreshSequence.current + 1;
    refreshSequence.current = requestId;
    if (showSpinner) {
      refreshAbort.current?.abort();
    }

    const controller = new AbortController();
    refreshAbort.current = controller;
    refreshInFlight.current = true;

    if (showSpinner) {
      setIsRefreshing(true);
    }

    try {
      const response = await fetch(`${API_URL}/tasks`, {
        cache: "no-store",
        signal: controller.signal,
      });
      if (!response.ok) {
        throw new Error(await response.text());
      }

      const nextTasks = (await response.json()) as Task[];
      if (requestId !== refreshSequence.current) {
        return;
      }

      setTasks(nextTasks);
      setSelectedId((current) => current ?? nextTasks[0]?.id ?? null);
      setError((current) => {
        const isBackendError =
          current === "Failed to fetch" || current === "Backend unavailable";
        if (showSpinner || errorSource.current === "backend" || isBackendError) {
          errorSource.current = null;
          return null;
        }

        return current;
      });
    } catch (reason) {
      if (reason instanceof Error && reason.name === "AbortError") {
        return;
      }

      errorSource.current = "backend";
      setError(reason instanceof Error ? reason.message : "Backend unavailable");
    } finally {
      if (refreshAbort.current === controller) {
        refreshAbort.current = null;
        refreshInFlight.current = false;
      }
      if (showSpinner && requestId === refreshSequence.current) {
        setIsRefreshing(false);
      }
    }
  }, []);

  useEffect(() => {
    async function loadRunners() {
      try {
        const response = await fetch(`${API_URL}/runners`, { cache: "no-store" });
        if (response.ok) {
          setRunners((await response.json()) as RunnerInfo[]);
        }
      } catch {
        setRunners([]);
      }
    }

    void loadRunners();
    const initial = window.setTimeout(() => {
      void refreshTasks(false);
    }, 0);
    const interval = window.setInterval(() => {
      void refreshTasks(false);
    }, 1500);

    return () => {
      window.clearTimeout(initial);
      window.clearInterval(interval);
      refreshAbort.current?.abort();
    };
  }, [refreshTasks]);

  async function createAndRun(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setIsSubmitting(true);
    errorSource.current = null;
    setError(null);

    try {
      const createResponse = await fetch(`${API_URL}/tasks`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          title: form.title.trim() || deriveTitle(form.prompt),
          prompt: form.prompt,
          runner: form.runner,
          command: form.runner === "shell" ? form.command : undefined,
          workspace: form.workspace,
          budget_minutes: form.budgetMinutes,
          policy: {
            allow_network: form.allowNetwork,
            allow_git_write: form.allowGitWrite,
            allow_secrets: form.allowSecrets,
            require_approval: form.requireApproval,
            blocked_commands: splitBlockedCommands(form.blockedCommands),
          } satisfies TaskPolicy,
        }),
      });

      if (!createResponse.ok) {
        throw new Error(await createResponse.text());
      }

      const created = (await createResponse.json()) as Task;
      setSelectedId(created.id);

      const startResponse = await fetch(`${API_URL}/tasks/${created.id}/start`, {
        method: "POST",
      });
      if (!startResponse.ok) {
        const message = await startResponse.text();
        await refreshTasks();

        if (message.toLowerCase().includes("approval")) {
          errorSource.current = null;
          setError(null);
          return;
        }

        throw new Error(message);
      }

      await refreshTasks();
    } catch (reason) {
      errorSource.current = "action";
      setError(reason instanceof Error ? reason.message : "Could not create task");
    } finally {
      setIsSubmitting(false);
    }
  }

  async function taskAction(taskId: string, action: "start" | "stop") {
    errorSource.current = null;
    setError(null);
    try {
      const response = await fetch(`${API_URL}/tasks/${taskId}/${action}`, {
        method: "POST",
      });
      if (!response.ok) {
        throw new Error(await response.text());
      }
      await refreshTasks();
    } catch (reason) {
      errorSource.current = "action";
      setError(reason instanceof Error ? reason.message : "Task action failed");
    }
  }

  async function approveTask(taskId: string) {
    errorSource.current = null;
    setError(null);
    try {
      const response = await fetch(`${API_URL}/tasks/${taskId}/approve`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ note: "Approved in control plane", start: true }),
      });
      if (!response.ok) {
        throw new Error(await response.text());
      }
      await refreshTasks();
    } catch (reason) {
      errorSource.current = "action";
      setError(reason instanceof Error ? reason.message : "Approval failed");
    }
  }

  async function replyTask(taskId: string, message: string) {
    errorSource.current = null;
    setError(null);
    try {
      const response = await fetch(`${API_URL}/tasks/${taskId}/reply`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ message }),
      });
      if (!response.ok) {
        throw new Error(await response.text());
      }
      await refreshTasks();
    } catch (reason) {
      errorSource.current = "action";
      setError(reason instanceof Error ? reason.message : "Reply failed");
    }
  }

  async function worktreeAction(taskId: string, action: "merge" | "cleanup") {
    errorSource.current = null;
    setError(null);
    try {
      const response = await fetch(
        `${API_URL}/tasks/${taskId}/worktree/${action}`,
        { method: "POST" },
      );
      if (!response.ok) {
        throw new Error(await response.text());
      }
      await refreshTasks();
    } catch (reason) {
      errorSource.current = "action";
      setError(reason instanceof Error ? reason.message : "Worktree action failed");
    }
  }

  return (
    <main className="min-h-screen bg-[#f4f5f6] text-slate-950">
      <header className="sticky top-0 z-20 border-b border-slate-200/80 bg-[#fbfcfc]/92 backdrop-blur-xl">
        <div className="mx-auto flex max-w-[1500px] items-center justify-between gap-5 px-6 py-4">
          <div className="min-w-0">
            <p className="text-[11px] font-semibold uppercase tracking-[0.24em] text-slate-500">
              openRuntime
            </p>
            <h1 className="mt-1 text-xl font-semibold tracking-normal">
              Runtime Control Plane
            </h1>
          </div>

          <div className="flex items-center gap-3">
            <StatusStrip stats={stats} />
            <RunnerStrip runners={runners} />
            <button
              title="Refresh"
              onClick={() => void refreshTasks()}
              className="grid size-10 place-items-center rounded-md border border-slate-200 bg-white/80 text-slate-600 shadow-[0_1px_1px_rgba(15,23,42,0.04)] transition hover:border-slate-300 hover:text-slate-950"
            >
              <RefreshCw
                className={`size-4 ${isRefreshing ? "animate-spin" : ""}`}
              />
            </button>
          </div>
        </div>
      </header>

      <div className="mx-auto max-w-[1500px] space-y-4 px-6 py-4">
        <CommandComposer
          form={form}
          runners={runners}
          error={error}
          isSubmitting={isSubmitting}
          onSubmit={createAndRun}
          onFormChange={setForm}
        />

        <section className="grid min-h-[calc(100vh-250px)] gap-4 xl:grid-cols-[340px_minmax(0,1fr)_300px]">
          <AgentRoster
            tasks={tasks}
            selectedTask={selectedTask}
            onSelect={setSelectedId}
          />
          <SessionTimeline
            key={selectedTask?.id ?? "no-session"}
            task={selectedTask}
            onStart={() => selectedTask && taskAction(selectedTask.id, "start")}
            onStop={() => selectedTask && taskAction(selectedTask.id, "stop")}
            onApprove={() => selectedTask && approveTask(selectedTask.id)}
            onReply={(message) =>
              selectedTask ? replyTask(selectedTask.id, message) : undefined
            }
            onMergeWorktree={() =>
              selectedTask && worktreeAction(selectedTask.id, "merge")
            }
            onCleanupWorktree={() =>
              selectedTask && worktreeAction(selectedTask.id, "cleanup")
            }
          />
          <SessionInspector task={selectedTask} />
        </section>
      </div>
    </main>
  );
}

function StatusStrip({
  stats,
}: {
  stats: { total: number; running: number; blocked: number; done: number };
}) {
  return (
    <div className="hidden items-center gap-1 rounded-md border border-slate-200 bg-white/70 px-2 py-1.5 md:flex">
      <MiniStat label="All" value={stats.total} />
      <MiniStat label="Run" value={stats.running} />
      <MiniStat label="Done" value={stats.done} />
      <MiniStat label="Attention" value={stats.blocked} tone="warn" />
    </div>
  );
}

function MiniStat({
  label,
  value,
  tone,
}: {
  label: string;
  value: number;
  tone?: "warn";
}) {
  return (
    <span
      className={`rounded px-2 py-1 text-xs ${
        tone === "warn" && value > 0
          ? "bg-[#fff7df] text-amber-800"
          : "text-slate-500"
      }`}
    >
      {label} <span className="font-semibold text-slate-950">{value}</span>
    </span>
  );
}

function RunnerStrip({ runners }: { runners: RunnerInfo[] }) {
  if (runners.length === 0) {
    return null;
  }

  return (
    <div className="hidden items-center gap-2 rounded-md border border-slate-200 bg-white/70 px-2 py-1.5 lg:flex">
      {runners.map((runner) => (
        <span
          key={runner.runner}
          className="flex items-center gap-1.5 rounded px-2 py-1 text-xs text-slate-500"
        >
          <span
            className={`size-2 rounded-full ${
              runner.available ? "bg-slate-500" : "bg-slate-300"
            }`}
          />
          {runnerLabels[runner.runner]}
        </span>
      ))}
    </div>
  );
}

function CommandComposer({
  form,
  runners,
  error,
  isSubmitting,
  onSubmit,
  onFormChange,
}: {
  form: FormState;
  runners: RunnerInfo[];
  error: string | null;
  isSubmitting: boolean;
  onSubmit: (event: FormEvent<HTMLFormElement>) => void;
  onFormChange: (value: FormState | ((current: FormState) => FormState)) => void;
}) {
  const [showGuardrails, setShowGuardrails] = useState(false);
  const [showRunners, setShowRunners] = useState(false);
  const [showWorkspaces, setShowWorkspaces] = useState(false);
  const [isPickingWorkspace, setIsPickingWorkspace] = useState(false);
  const [workspaceError, setWorkspaceError] = useState<string | null>(null);
  const [projects, setProjects] = useState<WorkspaceProject[]>(() =>
    uniqueProjects([{ name: workspaceName(form.workspace), path: form.workspace }]),
  );
  const guardrailsMenuRef = useRef<HTMLDivElement>(null);
  const runnerMenuRef = useRef<HTMLDivElement>(null);
  const workspaceMenuRef = useRef<HTMLDivElement>(null);
  const permissionPreset = getPermissionPreset(form);
  const runner = runners.find((item) => item.runner === form.runner);

  useEffect(() => {
    if (!showGuardrails && !showRunners && !showWorkspaces) {
      return;
    }

    function handlePointerDown(event: PointerEvent) {
      const target = event.target as Node;
      const clickedGuardrails = guardrailsMenuRef.current?.contains(target);
      const clickedRunner = runnerMenuRef.current?.contains(target);
      const clickedWorkspace = workspaceMenuRef.current?.contains(target);

      if (!clickedGuardrails && !clickedRunner && !clickedWorkspace) {
        setShowGuardrails(false);
        setShowRunners(false);
        setShowWorkspaces(false);
      }
    }

    function handleKeyDown(event: KeyboardEvent) {
      if (event.key === "Escape") {
        setShowGuardrails(false);
        setShowRunners(false);
        setShowWorkspaces(false);
      }
    }

    document.addEventListener("pointerdown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);

    return () => {
      document.removeEventListener("pointerdown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [showGuardrails, showRunners, showWorkspaces]);

  useEffect(() => {
    let cancelled = false;

    async function loadWorkspaces() {
      try {
        const response = await fetch(`${API_URL}/workspaces`, {
          cache: "no-store",
        });

        if (!response.ok) {
          throw new Error(await response.text());
        }

        const discovered = (await response.json()) as WorkspaceProject[];
        if (!cancelled) {
          setProjects(
            uniqueProjects([
              { name: workspaceName(form.workspace), path: form.workspace },
              ...discovered,
            ]),
          );
        }
      } catch {
        // Keep the current workspace available when discovery is unavailable.
      }
    }

    void loadWorkspaces();

    return () => {
      cancelled = true;
    };
  }, [form.workspace]);

  function selectWorkspace(path: string) {
    onFormChange((current) => ({ ...current, workspace: path }));
    setProjects(uniqueProjects([{ name: workspaceName(path), path }]));
    setWorkspaceError(null);
    setShowWorkspaces(false);
  }

  async function pickWorkspace() {
    setIsPickingWorkspace(true);
    setWorkspaceError(null);

    try {
      const response = await fetch(`${API_URL}/workspaces/pick`, {
        method: "POST",
      });

      if (!response.ok) {
        throw new Error(await response.text());
      }

      const project = (await response.json()) as WorkspaceProject | null;
      if (project) {
        onFormChange((current) => ({ ...current, workspace: project.path }));
        setProjects(uniqueProjects([project]));
        setShowWorkspaces(false);
      }
    } catch (reason) {
      setWorkspaceError(
        reason instanceof Error ? reason.message : "Could not open folder picker",
      );
    } finally {
      setIsPickingWorkspace(false);
    }
  }

  async function registerWorkspace(path: string) {
    setWorkspaceError(null);

    try {
      const response = await fetch(`${API_URL}/workspaces/register`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ path }),
      });

      if (!response.ok) {
        throw new Error(await response.text());
      }

      const project = (await response.json()) as WorkspaceProject;
      onFormChange((current) => ({ ...current, workspace: project.path }));
      setProjects((current) => uniqueProjects([project, ...current]));
      setShowWorkspaces(false);
    } catch (reason) {
      setWorkspaceError(
        reason instanceof Error ? reason.message : "Could not register workspace",
      );
    }
  }

  return (
    <section>
      <form onSubmit={onSubmit}>
        <div className="rounded-[30px] border border-slate-200/90 bg-white shadow-[0_24px_70px_rgba(15,23,42,0.07),0_1px_2px_rgba(15,23,42,0.05)]">
          <div className="px-6 pb-2 pt-5">
            <label className="sr-only">Goal</label>
            <textarea
              value={form.prompt}
              onChange={(event) =>
                onFormChange((current) => ({
                  ...current,
                  prompt: event.target.value,
                }))
              }
              rows={2}
              className="min-h-[74px] w-full resize-none border-0 bg-transparent p-0 text-[18px] leading-7 text-slate-950 outline-none placeholder:text-slate-400"
              placeholder="Ask an agent to work..."
            />
          </div>

          <div className="flex flex-wrap items-center gap-1.5 px-5 pb-4">
            <div ref={guardrailsMenuRef} className="relative">
              <button
                type="button"
                onClick={() => {
                  setShowGuardrails((current) => !current);
                  setShowRunners(false);
                  setShowWorkspaces(false);
                }}
                className={`flex h-9 items-center gap-2 rounded-full px-3 text-sm transition ${
                  permissionPreset === "full"
                    ? "bg-orange-50 text-[#e84d12] hover:bg-orange-100"
                    : "text-slate-500 hover:bg-slate-100"
                }`}
              >
                <ShieldCheck className="size-4" />
                {permissionButtonLabel(permissionPreset)}
                <ChevronDown className="size-4" />
              </button>

              {showGuardrails ? (
                <GuardrailsMenu
                  form={form}
                  onFormChange={onFormChange}
                  onClose={() => setShowGuardrails(false)}
                />
              ) : null}
            </div>

            <div className="min-w-0 flex-1" />

            <div ref={runnerMenuRef} className="relative">
              <button
                type="button"
                onClick={() => {
                  setShowRunners((current) => !current);
                  setShowGuardrails(false);
                  setShowWorkspaces(false);
                }}
                className="flex h-9 items-center gap-2 rounded-full px-3 text-sm text-slate-500 transition hover:bg-slate-100"
                title={runner?.command}
              >
                {runnerLabels[form.runner]}
                <ChevronDown className="size-4" />
              </button>

              {showRunners ? (
                <RunnerMenu
                  runners={runners}
                  value={form.runner}
                  onChange={(runner) => {
                    onFormChange((current) => ({ ...current, runner }));
                    setShowRunners(false);
                  }}
                />
              ) : null}
            </div>

            <label className="flex h-9 items-center gap-1 rounded-full px-2.5 text-sm text-slate-500 transition hover:bg-slate-100">
              <input
                type="number"
                min={1}
                max={240}
                value={form.budgetMinutes}
                onChange={(event) =>
                  onFormChange((current) => ({
                    ...current,
                    budgetMinutes: Number(event.target.value),
                  }))
                }
                className="w-8 border-0 bg-transparent text-right text-sm text-slate-700 outline-none"
                title="Budget in minutes"
              />
              <span>min</span>
            </label>

            <button
              type="submit"
              disabled={isSubmitting}
              className="grid size-11 place-items-center rounded-full bg-[#171b1f] text-white shadow-[0_10px_26px_rgba(15,23,42,0.18)] transition hover:bg-black disabled:cursor-not-allowed disabled:opacity-60"
              title="Run"
            >
              {isSubmitting ? (
                <Loader2 className="size-5 animate-spin" />
              ) : (
                <ArrowUp className="size-5" />
              )}
            </button>
          </div>
        </div>

        <div className="mt-3 flex flex-wrap items-center gap-3 px-5 text-sm text-slate-400">
          <div ref={workspaceMenuRef} className="relative min-w-[220px]">
            <button
              type="button"
              onClick={() => {
                setWorkspaceError(null);
                setShowWorkspaces((current) => !current);
                setShowGuardrails(false);
                setShowRunners(false);
              }}
              className="flex h-10 max-w-[360px] items-center gap-2 rounded-full bg-slate-100/80 px-4 text-slate-500 transition hover:bg-slate-200/70"
            >
              <Folder className="size-5" />
              <span className="truncate text-[15px]">
                {workspaceName(form.workspace)}
              </span>
              <ChevronDown className="size-4 shrink-0" />
            </button>

            {showWorkspaces ? (
              <WorkspaceMenu
                projects={projects}
                value={form.workspace}
                onSelect={selectWorkspace}
                onPick={pickWorkspace}
                onRegister={registerWorkspace}
                isPicking={isPickingWorkspace}
                error={workspaceError}
              />
            ) : null}
          </div>

          <span
            className={`flex items-center gap-1.5 text-xs ${
              runner?.available ? "text-slate-400" : "text-amber-600"
            }`}
            title={runner?.command}
          >
            <span
              className={`size-1.5 rounded-full ${
                runner?.available ? "bg-slate-400" : "bg-amber-500"
              }`}
            />
            {runnerLabels[form.runner]} {runner?.available ? "ready" : "missing"}
          </span>
        </div>

        {form.runner === "shell" ? (
          <div className="mt-3">
            <label className="sr-only">Shell command</label>
            <textarea
              value={form.command}
              onChange={(event) =>
                onFormChange((current) => ({
                  ...current,
                  command: event.target.value,
                }))
              }
              rows={2}
              className="input resize-none font-mono text-xs"
              placeholder="Shell command"
            />
          </div>
        ) : null}

        {error ? (
          <div className="mt-3 rounded-md border border-rose-200 bg-rose-50 px-3 py-2 text-sm text-rose-800">
            {error}
          </div>
        ) : null}
      </form>
    </section>
  );
}

function AgentRoster({
  tasks,
  selectedTask,
  onSelect,
}: {
  tasks: Task[];
  selectedTask?: Task;
  onSelect: (id: string) => void;
}) {
  const grouped = [
    ["Needs attention", tasks.filter((task) => task.status === "needs-input" || task.status === "failed")],
    ["Running", tasks.filter((task) => task.status === "running" || task.status === "queued")],
    ["Review", tasks.filter((task) => task.status === "ready-for-review" || Boolean(task.diff_stat))],
    ["Done", tasks.filter((task) => (task.status === "completed" && !task.diff_stat) || task.status === "stopped")],
  ] as const;

  return (
    <section className="rounded-[28px] border border-slate-200/80 bg-white p-5 shadow-[0_16px_40px_rgba(15,23,42,0.05)]">
      <div className="flex items-center justify-between gap-3">
        <h2 className="text-lg font-medium text-slate-400">Agent roster</h2>
        <span className="rounded-full bg-slate-100 px-2 py-0.5 text-xs text-slate-500">
          {tasks.length}
        </span>
      </div>

      <div className="mt-5 max-h-[calc(100vh-300px)] overflow-auto">
        {tasks.length === 0 ? (
          <div className="flex h-72 items-center justify-center text-sm text-slate-500">
            No sessions yet
          </div>
        ) : (
          grouped.map(([label, group]) =>
            group.length > 0 ? (
              <div key={label} className="mb-5">
                <div className="px-1 pb-2 text-[13px] font-medium text-slate-400">
                  {label}
                </div>
                <div className="space-y-1">
                  {group.map((task) => (
                    <RosterItem
                      key={task.id}
                      task={task}
                      selected={selectedTask?.id === task.id}
                      onSelect={() => onSelect(task.id)}
                    />
                  ))}
                </div>
              </div>
            ) : null,
          )
        )}
      </div>
    </section>
  );
}

function RosterItem({
  task,
  selected,
  onSelect,
}: {
  task: Task;
  selected: boolean;
  onSelect: () => void;
}) {
  const latest = latestReadableEvent(task);

  return (
    <button
      onClick={onSelect}
      className={`block w-full rounded-2xl px-3 py-3 text-left transition ${
        selected
          ? "bg-slate-100 text-slate-950"
          : "hover:bg-slate-50"
      }`}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span
              className={`size-2 rounded-full ${statusDotClass(task.status)}`}
            />
            <h3 className="truncate text-[15px] font-medium text-slate-800">{task.title}</h3>
          </div>
          <p
            className="mt-1 line-clamp-2 text-xs leading-5 text-slate-500"
          >
            {latest?.message ?? task.prompt}
          </p>
        </div>
        <div
          className="shrink-0 text-right text-[11px] text-slate-500"
        >
          <div>{formatTime(task.updated_at)}</div>
          <div className="mt-1">{runnerLabels[task.runner]}</div>
        </div>
      </div>
    </button>
  );
}

function SessionTimeline({
  task,
  onStart,
  onStop,
  onApprove,
  onReply,
  onMergeWorktree,
  onCleanupWorktree,
}: {
  task?: Task;
  onStart: () => void;
  onStop: () => void;
  onApprove: () => void;
  onReply: (message: string) => void;
  onMergeWorktree: () => void;
  onCleanupWorktree: () => void;
}) {
  const [pendingConfirmation, setPendingConfirmation] =
    useState<ConfirmableTaskAction | null>(null);

  if (!task) {
    return (
      <section className="flex items-center justify-center rounded-[28px] border border-slate-200 bg-white text-sm text-slate-500 shadow-[0_16px_40px_rgba(15,23,42,0.05)]">
        Select a session
      </section>
    );
  }

  const canStart =
    task.status !== "running" &&
    (task.status !== "needs-input" ||
      (Boolean(task.approved_at) && !task.current_attempt));
  const canStop =
    task.status === "running" ||
    (task.status === "needs-input" && Boolean(task.current_attempt));
  const latest = latestReadableEvent(task);
  const confirmation = pendingConfirmation
    ? confirmationDetails(task, pendingConfirmation)
    : null;

  function confirmPendingAction() {
    const action = pendingConfirmation;
    setPendingConfirmation(null);

    switch (action) {
      case "stop":
        onStop();
        break;
      case "approve":
        onApprove();
        break;
      case "merge":
        onMergeWorktree();
        break;
      case "cleanup":
        onCleanupWorktree();
        break;
    }
  }

  return (
    <section className="rounded-[28px] border border-slate-200/80 bg-white shadow-[0_16px_40px_rgba(15,23,42,0.05)]">
      <div className="px-6 py-5">
        <div className="flex items-start justify-between gap-4">
          <div className="min-w-0">
            <div className="flex items-center gap-2">
              <span className={`size-2.5 rounded-full ${statusDotClass(task.status)}`} />
              <h2 className="truncate text-[22px] font-semibold tracking-normal text-slate-950">{task.title}</h2>
              <StatusPill status={task.status} />
            </div>
            <p className="mt-2 text-[15px] leading-6 text-slate-500">{task.prompt}</p>
          </div>
          <div className="flex shrink-0 gap-2">
            <button
              title="Start task"
              disabled={!canStart}
              onClick={onStart}
              className="grid size-10 place-items-center rounded-2xl border border-slate-200 bg-white text-slate-500 transition hover:bg-slate-50 hover:text-slate-950 disabled:cursor-not-allowed disabled:opacity-40"
            >
              <Play className="size-4" />
            </button>
            <button
              title="Stop task"
              disabled={!canStop}
              onClick={() => setPendingConfirmation("stop")}
              className="grid size-10 place-items-center rounded-2xl border border-slate-200 bg-white text-slate-500 transition hover:bg-rose-50 hover:text-rose-700 disabled:cursor-not-allowed disabled:opacity-40"
            >
              <CircleStop className="size-4" />
            </button>
          </div>
        </div>

        {latest ? (
          <div className="mt-5 rounded-2xl border border-slate-200 bg-slate-50/70 px-4 py-3">
            <div className="mb-1 text-xs font-medium text-slate-400">
              Latest signal
            </div>
            <pre className="line-clamp-3 whitespace-pre-wrap break-words font-mono text-xs leading-5 text-slate-800">
              {latest.message}
            </pre>
          </div>
        ) : null}

        <SessionInputPanel
          task={task}
          onApprove={() => setPendingConfirmation("approve")}
          onReply={onReply}
        />

        {confirmation ? (
          <ActionConfirmationPanel
            title={confirmation.title}
            body={confirmation.body}
            confirmLabel={confirmation.confirmLabel}
            onCancel={() => setPendingConfirmation(null)}
            onConfirm={confirmPendingAction}
          />
        ) : null}

        <DiffReview
          key={task.id}
          task={task}
          onMerge={() => setPendingConfirmation("merge")}
          onCleanup={() => setPendingConfirmation("cleanup")}
        />
      </div>

      <div className="border-t border-slate-200/80 px-6 py-5">
        <h3 className="mb-4 text-lg font-medium text-slate-400">Event stream</h3>
        <div className="max-h-[calc(100vh-385px)] overflow-auto pr-1">
        <div className="space-y-3">
          {task.events.map((event) => (
            <EventItem key={event.id} event={event} />
          ))}
        </div>
        </div>
      </div>
    </section>
  );
}

function SessionInputPanel({
  task,
  onApprove,
  onReply,
}: {
  task: Task;
  onApprove: () => void;
  onReply: (message: string) => void;
}) {
  const [message, setMessage] = useState("");
  const canApprove =
    task.policy.require_approval &&
    !task.approved_at &&
    task.status === "needs-input";
  const canReply =
    task.runner === "shell" &&
    (task.status === "running" || task.status === "needs-input");

  if (!canApprove && !canReply) {
    return null;
  }

  return (
    <div className="mt-5 rounded-2xl border border-slate-200 bg-white px-4 py-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <div className="text-xs font-medium text-slate-400">
            Human handoff
          </div>
          <div className="mt-1 text-sm text-slate-700">
            {canApprove ? "Approval is required before this task can run." : "Send a reply into this session."}
          </div>
        </div>
        {canApprove ? (
          <button
            type="button"
            onClick={onApprove}
            className="rounded-md bg-[#171b1f] px-3 py-2 text-xs font-medium text-white transition hover:bg-black"
          >
            Approve and run
          </button>
        ) : null}
      </div>

      {canReply ? (
        <div className="mt-3 flex gap-2">
          <input
            value={message}
            onChange={(event) => setMessage(event.target.value)}
            placeholder="Reply to this agent"
            className="min-w-0 flex-1 rounded-xl border border-slate-200 bg-slate-50 px-3 py-2 text-sm text-slate-800 outline-none transition focus:border-slate-300 focus:bg-white"
          />
          <button
            type="button"
            disabled={!message.trim()}
            onClick={() => {
              const next = message.trim();
              setMessage("");
              onReply(next);
            }}
            className="rounded-md border border-slate-200 bg-white px-3 py-2 text-xs font-medium text-slate-600 transition hover:bg-slate-50 disabled:cursor-not-allowed disabled:opacity-50"
          >
            Send reply
          </button>
        </div>
      ) : null}
    </div>
  );
}

function ActionConfirmationPanel({
  title,
  body,
  confirmLabel,
  onCancel,
  onConfirm,
}: {
  title: string;
  body: string;
  confirmLabel: string;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div className="mt-5 rounded-2xl border border-amber-200 bg-[#fff8e7] px-4 py-3">
      <div className="text-xs font-medium text-amber-700">Confirm action</div>
      <div className="mt-1 text-sm font-medium text-amber-900">{title}</div>
      <p className="mt-1 text-sm leading-5 text-amber-800">{body}</p>
      <div className="mt-3 flex flex-wrap gap-2">
        <button
          type="button"
          onClick={onConfirm}
          className="rounded-md bg-[#171b1f] px-3 py-2 text-xs font-medium text-white transition hover:bg-black"
        >
          {confirmLabel}
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="rounded-md border border-amber-200 bg-white px-3 py-2 text-xs font-medium text-amber-800 transition hover:bg-amber-50"
        >
          Cancel
        </button>
      </div>
    </div>
  );
}

function DiffReview({
  task,
  onMerge,
  onCleanup,
}: {
  task: Task;
  onMerge: () => void;
  onCleanup: () => void;
}) {
  const [diff, setDiff] = useState<TaskDiff | null>(null);
  const [isLoading, setIsLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const workspaceIsolationDisabled = isWorkspaceIsolationDisabled(task);
  const hasReviewSignal = Boolean(
    task.diff_stat || task.worktree_path || workspaceIsolationDisabled,
  );

  if (!hasReviewSignal) {
    return null;
  }

  async function loadDiff() {
    setIsLoading(true);
    setError(null);

    try {
      const response = await fetch(`${API_URL}/tasks/${task.id}/diff`, {
        cache: "no-store",
      });
      if (!response.ok) {
        throw new Error(await response.text());
      }

      setDiff((await response.json()) as TaskDiff);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "Could not load diff");
    } finally {
      setIsLoading(false);
    }
  }

  return (
    <div className="mt-5 rounded-2xl border border-slate-200 bg-white px-4 py-3">
      <div className="flex items-center justify-between gap-3">
        <div>
          <div className="text-xs font-medium text-slate-400">
            Review surface
          </div>
          <div className="mt-1 flex items-center gap-2 text-sm text-slate-700">
            <GitBranch className="size-4 text-slate-500" />
            {task.worktree_path ? "Isolated worktree" : "Direct workspace"}
          </div>
        </div>
        <div className="flex flex-wrap gap-2">
          <button
            type="button"
            onClick={() => void loadDiff()}
            disabled={isLoading || Boolean(task.worktree_cleaned_at)}
            className="rounded-md border border-slate-200 bg-white px-3 py-2 text-xs font-medium text-slate-600 transition hover:bg-slate-50 disabled:cursor-not-allowed disabled:opacity-50"
          >
            {isLoading ? "Loading" : diff ? "Refresh diff" : "Load diff"}
          </button>
          {task.worktree_path ? (
            <>
              <button
                type="button"
                onClick={onMerge}
                disabled={
                  !task.diff_stat ||
                  Boolean(task.worktree_merged_at) ||
                  Boolean(task.worktree_cleaned_at)
                }
                className="rounded-md border border-slate-200 bg-white px-3 py-2 text-xs font-medium text-slate-600 transition hover:bg-slate-50 disabled:cursor-not-allowed disabled:opacity-50"
              >
                {task.worktree_merged_at ? "Merged" : "Merge"}
              </button>
              <button
                type="button"
                onClick={onCleanup}
                disabled={task.status === "running" || Boolean(task.worktree_cleaned_at)}
                className="rounded-md border border-slate-200 bg-white px-3 py-2 text-xs font-medium text-slate-600 transition hover:bg-slate-50 disabled:cursor-not-allowed disabled:opacity-50"
              >
                {task.worktree_cleaned_at ? "Cleaned" : "Cleanup"}
              </button>
            </>
          ) : null}
        </div>
      </div>

      {workspaceIsolationDisabled ? (
        <WorktreeIsolationWarning className="mt-3" />
      ) : null}

      {task.diff_stat ? (
        <pre className="mt-3 max-h-40 overflow-auto whitespace-pre-wrap break-words rounded-xl bg-slate-50 p-3 font-mono text-xs leading-5 text-slate-800">
          {task.diff_stat}
        </pre>
      ) : (
        <div className="mt-3 rounded-xl bg-slate-50 p-3 text-sm text-slate-500">
          No captured changes yet
        </div>
      )}

      {diff?.patch ? (
        <pre className="mt-3 max-h-72 overflow-auto whitespace-pre-wrap break-words rounded-xl bg-[#101418] p-3 font-mono text-xs leading-5 text-slate-100">
          {diff.patch}
        </pre>
      ) : null}

      {error ? (
        <div className="mt-3 rounded-xl border border-rose-200 bg-rose-50 px-3 py-2 text-sm text-rose-700">
          {error}
        </div>
      ) : null}
    </div>
  );
}

function SessionInspector({ task }: { task?: Task }) {
  if (!task) {
    return (
      <aside className="flex items-center justify-center rounded-[28px] border border-slate-200 bg-white text-sm text-slate-500 shadow-[0_16px_40px_rgba(15,23,42,0.05)]">
        No session selected
      </aside>
    );
  }

  const latest = latestReadableEvent(task);
  const isolated = Boolean(task.worktree_path);
  const workspaceIsolationDisabled = isWorkspaceIsolationDisabled(task);
  const policy = task.effective_policy ?? task.policy;
  const policyFrozen = Boolean(task.effective_policy);
  const networkMode = policy.allow_network
    ? "enabled"
    : (policy.network_mode ?? "disabled");
  const currentAttempt = task.current_attempt;
  const blockedReason = structuredBlockedReason(task);
  const progress = [
    { label: "Create agent session", done: true },
    {
      label: isolated ? "Create isolated worktree" : "Check workspace isolation",
      done: isolated || workspaceIsolationDisabled,
    },
    {
      label: `Start ${runnerLabels[task.runner]} runner`,
      done: task.events.some((event) =>
        event.message.toLowerCase().includes("starting"),
      ),
    },
    {
      label: "Stream execution events",
      done: task.events.some(
        (event) => event.kind === "stdout" || event.kind === "stderr",
      ),
    },
    {
      label: progressLabel(task.status),
      done: terminalStatuses.has(task.status),
    },
  ];

  return (
    <aside className="rounded-[28px] border border-slate-200/80 bg-white p-5 text-slate-800 shadow-[0_16px_40px_rgba(15,23,42,0.05)]">
      <div className="flex items-center justify-between gap-3">
        <h2 className="text-lg font-medium text-slate-400">Progress</h2>
        <Pin className="size-5 text-slate-400" />
      </div>

      <div className="mt-5 space-y-3">
        {progress.map((item) => (
          <ProgressItem key={item.label} done={item.done} label={item.label} />
        ))}
      </div>

      <InspectorSection title="Runtime">
        <InspectorItem
          icon={<Terminal className="size-5" />}
          label={runnerLabels[task.runner]}
          value={terminalLabel(task)}
          mono
        />
        <InspectorItem
          icon={<Terminal className="size-5" />}
          label="Runner session"
          value={task.runner_session_id ?? "No runner session id captured yet"}
          mono={Boolean(task.runner_session_id)}
        />
        <InspectorItem
          icon={<Folder className="size-5" />}
          label="Execution workspace"
          value={task.execution_workspace ?? "Not prepared yet"}
          mono={Boolean(task.execution_workspace)}
        />
        <InspectorItem
          icon={<Folder className="size-5" />}
          label={workspaceName(task.workspace)}
          value={task.workspace}
        />
        <InspectorItem
          icon={<GitBranch className="size-5" />}
          label={isolated ? "Isolated worktree" : "No worktree isolation"}
          value={
            task.worktree_path ??
            "Choose or register a git repo workspace to isolate future runs."
          }
        />
        {workspaceIsolationDisabled ? <WorktreeIsolationWarning /> : null}
        {task.worktree_merged_at ? (
          <InspectorItem
            icon={<GitBranch className="size-5" />}
            label="Worktree merged"
            value={formatTime(task.worktree_merged_at)}
          />
        ) : null}
        {task.worktree_cleaned_at ? (
          <InspectorItem
            icon={<GitBranch className="size-5" />}
            label="Worktree cleaned"
            value={formatTime(task.worktree_cleaned_at)}
          />
        ) : null}
        <InspectorItem
          icon={<Clock className="size-5" />}
          label={`${task.budget_minutes} minute budget`}
          value={budgetState(task)}
        />
        <InspectorItem
          icon={<Gauge className="size-5" />}
          label="Cost ledger"
          value={costLedgerSummary(task.cost_ledger)}
        />
        <InspectorItem
          icon={<Activity className="size-5" />}
          label={
            currentAttempt
              ? `Attempt ${currentAttempt.attempt_number} ${currentAttempt.status}`
              : "No attempt yet"
          }
          value={
            currentAttempt
              ? currentAttempt.summary ??
                currentAttempt.execution_workspace ??
                "Attempt is active"
              : `${task.attempts.length} attempts recorded`
          }
        />
        {blockedReason ? (
          <InspectorItem
            icon={<Ban className="size-5" />}
            label="Blocked reason"
            value={blockedReason}
          />
        ) : null}
      </InspectorSection>

      <InspectorSection title="Guardrails">
        <InspectorItem
          icon={<ShieldCheck className="size-5" />}
          label={policyFrozen ? "Effective policy" : "Draft policy"}
          value={policyFrozen ? "Frozen at task start" : "Will freeze when started"}
        />
        <GuardrailItem
          icon={<Wifi className="size-5" />}
          label={`Network ${networkMode}`}
          enabled={networkMode !== "disabled"}
        />
        <GuardrailItem
          icon={<GitBranch className="size-5" />}
          label="Git write"
          enabled={policy.allow_git_write}
        />
        <GuardrailItem
          icon={<KeyRound className="size-5" />}
          label="Secrets"
          enabled={policy.allow_secrets}
        />
        <GuardrailItem
          icon={<Ban className="size-5" />}
          label="Approval gate"
          enabled={policy.require_approval}
          enabledLabel="required"
          disabledLabel="not required"
        />
        <InspectorItem
          icon={<Shield className="size-5" />}
          label={`${policy.blocked_commands.length} blocked fragments`}
          value={policy.blocked_commands.join(", ") || "None"}
        />
        <InspectorItem
          icon={<Folder className="size-5" />}
          label={`${policy.allowed_workspaces?.length ?? 0} allowed workspaces`}
          value={policy.allowed_workspaces?.join(", ") || "Current workspace"}
        />
        <InspectorItem
          icon={<Shield className="size-5" />}
          label={`${policy.allowed_mcp_tools?.length ?? 0} allowed tools`}
          value={policy.allowed_mcp_tools?.join(", ") || "No MCP/tool allowlist"}
        />
      </InspectorSection>

      <InspectorSection title="Signals">
        <InspectorItem
          icon={<Activity className="size-5" />}
          label={latest?.kind ? latest.kind.toUpperCase() : "No signal yet"}
          value={latest?.message ?? "The runner has not emitted output."}
        />
        <InspectorItem
          icon={<Gauge className="size-5" />}
          label={`${task.events.length} persisted events`}
          value={eventBreakdown(task)}
        />
        <InspectorItem
          icon={<GitBranch className="size-5" />}
          label={task.diff_stat ? "Diff captured" : "No diff captured"}
          value={task.diff_stat ?? task.base_commit ?? undefined}
        />
      </InspectorSection>
    </aside>
  );
}

function WorktreeIsolationWarning({ className = "" }: { className?: string }) {
  return (
    <div
      className={`rounded-xl border border-amber-200 bg-[#fff8e7] px-3 py-2 text-xs leading-5 text-amber-800 ${className}`}
    >
      This workspace is not inside a git repository, so worktree isolation is
      disabled. Select or register a git repo workspace for isolated future
      runs.
    </div>
  );
}

function ProgressItem({ done, label }: { done: boolean; label: string }) {
  return (
    <div className="flex items-start gap-3 text-[15px] leading-6 text-slate-500">
      <span
        className={`mt-1 grid size-5 shrink-0 place-items-center rounded-full border ${
          done
            ? "border-slate-400 bg-slate-100 text-slate-600"
            : "border-slate-300 bg-white"
        }`}
      >
        {done ? <Check className="size-3.5" /> : null}
      </span>
      <span className={done ? "text-slate-700" : "text-slate-500"}>
        {label}
      </span>
    </div>
  );
}

function InspectorSection({
  title,
  children,
}: {
  title: string;
  children: ReactNode;
}) {
  return (
    <section className="mt-6 border-t border-slate-200/80 pt-5">
      <h3 className="text-lg font-medium text-slate-400">{title}</h3>
      <div className="mt-4 space-y-3">{children}</div>
    </section>
  );
}

function InspectorItem({
  icon,
  label,
  value,
  mono,
}: {
  icon: ReactNode;
  label: string;
  value?: string;
  mono?: boolean;
}) {
  return (
    <div className="flex min-w-0 items-start gap-3">
      <span className="mt-0.5 shrink-0 text-slate-500">{icon}</span>
      <div className="min-w-0">
        <div
          className={`truncate text-[15px] leading-6 text-slate-800 ${
            mono ? "font-mono" : ""
          }`}
        >
          {label}
        </div>
        {value ? (
          <div className="mt-0.5 line-clamp-2 break-words text-xs leading-5 text-slate-400">
            {value}
          </div>
        ) : null}
      </div>
    </div>
  );
}

function GuardrailItem({
  icon,
  label,
  enabled,
  enabledLabel = "allow",
  disabledLabel = "deny",
}: {
  icon: ReactNode;
  label: string;
  enabled: boolean;
  enabledLabel?: string;
  disabledLabel?: string;
}) {
  return (
    <div className="flex min-w-0 items-center gap-3">
      <span className="shrink-0 text-slate-500">{icon}</span>
      <div className="min-w-0 flex-1 truncate text-[15px] text-slate-800">
        {label}
      </div>
      <span
        className={`rounded-full px-2 py-0.5 text-xs ${
          enabled
            ? "bg-orange-50 text-[#e84d12]"
            : "bg-slate-100 text-slate-500"
        }`}
      >
        {enabled ? enabledLabel : disabledLabel}
      </span>
    </div>
  );
}

function progressLabel(status: TaskStatus) {
  switch (status) {
    case "completed":
      return "Complete and ready for review";
    case "failed":
      return "Needs attention";
    case "needs-input":
      return "Waiting for input";
    case "running":
      return "Running session";
    case "queued":
      return "Queued for execution";
    case "ready-for-review":
      return "Ready for review";
    case "stopped":
      return "Stopped";
  }
}

function terminalLabel(task: Task) {
  if (task.runner === "shell") {
    return task.command || "/bin/sh -lc";
  }

  return task.runner === "codex" ? "codex exec" : "claude -p";
}

function budgetState(task: Task) {
  if (task.status === "running") {
    return "Budget timer active";
  }

  return terminalStatuses.has(task.status)
    ? `Session ${task.status}`
    : "Budget reserved for run";
}

function eventBreakdown(task: Task) {
  const counts = task.events.reduce(
    (accumulator, event) => ({
      ...accumulator,
      [event.kind]: accumulator[event.kind] + 1,
    }),
    { lifecycle: 0, stdout: 0, stderr: 0, diff: 0, input: 0, error: 0 } as Record<EventKind, number>,
  );

  return `Lifecycle ${counts.lifecycle}, stdout ${counts.stdout}, stderr ${counts.stderr}, inputs ${counts.input}, diffs ${counts.diff}, errors ${counts.error}`;
}

function costLedgerSummary(ledger: CostLedger) {
  const seconds = Math.round(ledger.runtime_millis / 100) / 10;
  return `${seconds}s runtime, ${ledger.input_tokens} input tokens, ${ledger.output_tokens} output tokens, ${ledger.tool_calls} tool calls, ${ledger.estimated_cents}c estimated`;
}

function structuredBlockedReason(task: Task) {
  if (!["needs-input", "failed", "stopped"].includes(task.status)) {
    return null;
  }

  const currentAttemptId = task.current_attempt?.id ?? null;
  const event = [...task.events].reverse().find((event) => {
    const category = event.metadata?.category;
    const belongsToCurrentAttempt =
      !currentAttemptId ||
      !event.attempt_id ||
      event.attempt_id === currentAttemptId;
    return belongsToCurrentAttempt && (
      category === "needs-input" ||
      category === "attempt-stopped" ||
      category === "attempt-failed" ||
      category === "attempt-monitor-error"
    );
  });

  if (!event) {
    return null;
  }

  const reason = event.metadata?.reason;
  return typeof reason === "string" && reason.trim().length > 0
    ? reason
    : event.message;
}

function GuardrailsMenu({
  form,
  onFormChange,
  onClose,
}: {
  form: FormState;
  onFormChange: (value: FormState | ((current: FormState) => FormState)) => void;
  onClose: () => void;
}) {
  const selected = getPermissionPreset(form);
  const options: {
    value: PermissionPreset;
    label: string;
    icon: ReactNode;
  }[] = [
    {
      value: "default",
      label: "Default permissions",
      icon: <Hand className="size-5" />,
    },
    {
      value: "review",
      label: "Auto-review",
      icon: <Shield className="size-5" />,
    },
    {
      value: "full",
      label: "Full access",
      icon: <ShieldCheck className="size-5" />,
    },
  ];

  return (
    <div className="absolute left-0 top-full z-40 mt-2 w-[300px] rounded-[22px] border border-slate-200 bg-white p-2 shadow-[0_22px_70px_rgba(15,23,42,0.16),0_1px_2px_rgba(15,23,42,0.08)]">
      <div className="space-y-1">
        {options.map((option) => (
          <button
            key={option.value}
            type="button"
            onClick={() => {
              onFormChange((current) =>
                applyPermissionPreset(current, option.value),
              );
              onClose();
            }}
            className={`flex h-12 w-full items-center gap-3 rounded-2xl px-3 text-left text-[15px] transition ${
              selected === option.value
                ? "bg-slate-100 text-slate-950"
                : "text-slate-700 hover:bg-slate-50"
            }`}
          >
            <span className="text-slate-500">{option.icon}</span>
            <span className="min-w-0 flex-1">{option.label}</span>
            {selected === option.value ? <Check className="size-5" /> : null}
          </button>
        ))}
      </div>

      <div className="mt-2 border-t border-slate-100 px-2 pb-2 pt-3">
        <label className="block text-xs font-medium uppercase tracking-[0.12em] text-slate-400">
          Blocked commands
        </label>
        <input
          value={form.blockedCommands}
          onChange={(event) =>
            onFormChange((current) => ({
              ...current,
              blockedCommands: event.target.value,
            }))
          }
          className="mt-2 w-full rounded-xl border border-slate-200 bg-slate-50 px-3 py-2 font-mono text-xs text-slate-700 outline-none transition focus:border-slate-300 focus:bg-white"
        />
      </div>
    </div>
  );
}

function WorkspaceMenu({
  projects,
  value,
  onSelect,
  onPick,
  onRegister,
  isPicking,
  error,
}: {
  projects: WorkspaceProject[];
  value: string;
  onSelect: (path: string) => void;
  onPick: () => void;
  onRegister: (path: string) => void;
  isPicking: boolean;
  error: string | null;
}) {
  const [query, setQuery] = useState("");
  const [pathEntry, setPathEntry] = useState("");
  const filteredProjects = projects.filter((project) => {
    const searchTarget = `${project.name} ${project.path}`.toLowerCase();
    return searchTarget.includes(query.trim().toLowerCase());
  });

  return (
    <div className="absolute left-0 top-full z-40 mt-2 w-[360px] rounded-[22px] border border-slate-200 bg-white p-3 shadow-[0_22px_70px_rgba(15,23,42,0.16),0_1px_2px_rgba(15,23,42,0.08)]">
      <label className="flex h-11 items-center gap-3 rounded-2xl px-3 text-slate-400">
        <Search className="size-5" />
        <input
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder="Search projects"
          className="min-w-0 flex-1 border-0 bg-transparent text-[17px] text-slate-900 outline-none placeholder:text-slate-400"
        />
      </label>

      <div className="mt-2 max-h-[340px] overflow-auto">
        {filteredProjects.length > 0 ? (
          filteredProjects.map((project) => (
            <button
              key={project.path}
              type="button"
              onClick={() => onSelect(project.path)}
              className={`flex h-12 w-full items-center gap-3 rounded-2xl px-3 text-left transition ${
                project.path === value
                  ? "bg-slate-100 text-slate-950"
                  : "text-slate-700 hover:bg-slate-50"
              }`}
            >
              <Folder className="size-5 shrink-0 text-slate-500" />
              <span className="min-w-0 flex-1 truncate text-[15px]">
                {project.name}
              </span>
              {project.path === value ? <Check className="size-5" /> : null}
            </button>
          ))
        ) : (
          <div className="px-3 py-5 text-sm text-slate-400">
            No matching workspaces
          </div>
        )}
      </div>

      <div className="mt-2 border-t border-slate-100 pt-2">
        <div className="flex gap-2 px-1 pb-2">
          <input
            value={pathEntry}
            onChange={(event) => setPathEntry(event.target.value)}
            placeholder="/absolute/path/to/repo"
            className="min-w-0 flex-1 rounded-xl border border-slate-200 bg-slate-50 px-3 py-2 font-mono text-xs text-slate-700 outline-none transition focus:border-slate-300 focus:bg-white"
          />
          <button
            type="button"
            disabled={!pathEntry.trim()}
            onClick={() => onRegister(pathEntry.trim())}
            className="rounded-xl border border-slate-200 bg-white px-3 py-2 text-xs font-medium text-slate-600 transition hover:bg-slate-50 disabled:cursor-not-allowed disabled:opacity-50"
          >
            Add
          </button>
        </div>
        <button
          type="button"
          onClick={onPick}
          disabled={isPicking}
          className="flex h-12 w-full items-center gap-3 rounded-2xl px-3 text-left text-slate-700 transition hover:bg-slate-50"
        >
          {isPicking ? (
            <Loader2 className="size-5 animate-spin text-slate-500" />
          ) : (
            <FolderPlus className="size-5 text-slate-500" />
          )}
          <span className="text-[15px]">
            {isPicking ? "Opening folder picker" : "Choose folder"}
          </span>
        </button>
        {error ? (
          <div className="px-3 pb-2 text-xs leading-5 text-rose-600">
            {error}
          </div>
        ) : null}
      </div>
    </div>
  );
}

function RunnerMenu({
  runners,
  value,
  onChange,
}: {
  runners: RunnerInfo[];
  value: RunnerKind;
  onChange: (runner: RunnerKind) => void;
}) {
  const runnerOptions: RunnerKind[] = ["codex", "claude-code", "shell"];

  return (
    <div className="absolute right-0 top-full z-40 mt-2 w-[250px] rounded-[22px] border border-slate-200 bg-white p-2 shadow-[0_22px_70px_rgba(15,23,42,0.16),0_1px_2px_rgba(15,23,42,0.08)]">
      {runnerOptions.map((runner) => {
        const info = runners.find((item) => item.runner === runner);
        return (
          <button
            key={runner}
            type="button"
            onClick={() => onChange(runner)}
            className={`flex h-12 w-full items-center gap-3 rounded-2xl px-3 text-left transition ${
              value === runner
                ? "bg-slate-100 text-slate-950"
                : "text-slate-700 hover:bg-slate-50"
            }`}
            title={info?.command}
          >
            <span
              className={`size-2 rounded-full ${
                info?.available ? "bg-slate-500" : "bg-amber-500"
              }`}
            />
            <span className="min-w-0 flex-1 text-[15px]">
              {runnerLabels[runner]}
            </span>
            {value === runner ? <Check className="size-5" /> : null}
          </button>
        );
      })}
    </div>
  );
}

function EventItem({ event }: { event: TaskEvent }) {
  return (
    <div className={`rounded-2xl border bg-white px-4 py-3 ${eventStyles[event.kind]}`}>
      <div className="flex items-center justify-between gap-3">
        <span className="text-xs font-semibold uppercase tracking-[0.16em]">
          {event.kind}
        </span>
        <span className="text-xs text-slate-400">{formatTime(event.created_at)}</span>
      </div>
      <pre className="mt-2 whitespace-pre-wrap break-words font-mono text-[13px] leading-6 text-slate-800">
        {event.message}
      </pre>
    </div>
  );
}

function applyPermissionPreset(form: FormState, preset: PermissionPreset) {
  switch (preset) {
    case "full":
      return {
        ...form,
        allowNetwork: true,
        allowGitWrite: true,
        allowSecrets: true,
        requireApproval: false,
        blockedCommands: "",
      };
    case "review":
      return {
        ...form,
        allowNetwork: false,
        allowGitWrite: false,
        allowSecrets: false,
        requireApproval: true,
        blockedCommands: DEFAULT_BLOCKED_COMMANDS,
      };
    case "default":
      return {
        ...form,
        allowNetwork: false,
        allowGitWrite: false,
        allowSecrets: false,
        requireApproval: false,
        blockedCommands: DEFAULT_BLOCKED_COMMANDS,
      };
  }
}

function getPermissionPreset(form: FormState): PermissionPreset {
  if (form.allowNetwork || form.allowGitWrite || form.allowSecrets) {
    return "full";
  }

  if (form.requireApproval) {
    return "review";
  }

  return "default";
}

function permissionButtonLabel(preset: PermissionPreset) {
  switch (preset) {
    case "full":
      return "Full access";
    case "review":
      return "Auto-review";
    case "default":
      return "Default";
  }
}

function StatusPill({ status }: { status: TaskStatus }) {
  return (
    <span
      className={`rounded-md border px-2 py-0.5 text-xs font-medium ${statusStyles[status]}`}
    >
      {status}
    </span>
  );
}

function splitBlockedCommands(value: string) {
  return value
    .split(/[,\n]/)
    .map((item) => item.trim())
    .filter(Boolean);
}

function workspaceName(path: string) {
  const normalized = path.trim().replace(/\/+$/, "");
  return normalized.split("/").filter(Boolean).at(-1) ?? "Local workspace";
}

function uniqueProjects(projects: WorkspaceProject[]) {
  const seen = new Set<string>();
  return projects.filter((project) => {
    if (seen.has(project.path)) {
      return false;
    }

    seen.add(project.path);
    return true;
  });
}

function latestReadableEvent(task: Task) {
  return [...task.events]
    .reverse()
    .find((event) => event.message.trim().length > 0);
}

function isWorkspaceIsolationDisabled(task: Task) {
  return (
    !task.worktree_path &&
    task.events.some((event) =>
      event.message.toLowerCase().includes("without worktree isolation"),
    )
  );
}

function confirmationDetails(task: Task, action: ConfirmableTaskAction) {
  switch (action) {
    case "stop":
      return {
        title: `Stop "${task.title}"`,
        body: "This terminates the running agent session for this task.",
        confirmLabel: "Stop task",
      };
    case "approve":
      return {
        title: `Approve and run "${task.title}"`,
        body: "This approves the pending policy gate and immediately starts the runner.",
        confirmLabel: "Approve and run",
      };
    case "merge":
      return {
        title: `Merge "${task.title}"`,
        body: "This applies the isolated worktree changes back into the selected workspace.",
        confirmLabel: "Merge worktree",
      };
    case "cleanup":
      return {
        title: `Cleanup "${task.title}"`,
        body: "This removes the isolated worktree. Review or merge useful changes before cleanup.",
        confirmLabel: "Cleanup worktree",
      };
  }
}

function statusDotClass(status: TaskStatus) {
  switch (status) {
    case "running":
      return "bg-slate-700";
    case "completed":
      return "bg-emerald-600";
    case "failed":
    case "needs-input":
      return "bg-amber-500";
    case "ready-for-review":
      return "bg-slate-500";
    case "queued":
      return "bg-slate-400";
    case "stopped":
      return "bg-slate-300";
  }
}

function deriveTitle(prompt: string) {
  const normalized = prompt.trim().replace(/\s+/g, " ");
  if (!normalized) {
    return "Untitled agent session";
  }

  return normalized.length > 56 ? `${normalized.slice(0, 53)}...` : normalized;
}

function formatTime(value: string) {
  return new Intl.DateTimeFormat("en", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  }).format(new Date(value));
}
