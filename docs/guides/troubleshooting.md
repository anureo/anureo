# anureo Troubleshooting

## Gather Minimum Information First

Record the anureo version/commit, command or entry point, effective working directory, session/instance ID, model/provider, error classification, and log file location. Do not paste API keys, cookies, full environment variables, or private source code.

## Configuration and Models

| Symptom | Troubleshooting Steps |
| --- | --- |
| Missing API key or auth failure | Check precedence: project `.env`, `~/.anureo/config.toml`, and shell environment; do not echo keys in the terminal. |
| Model not found | Run `anureo models`; check provider name, `--model` format, and whether the model is configured. |
| model/provider/tier conflict | Remove one of the overriding parameters, or explicitly choose the provider and model to use. |
| Request failed or rate-limited | Check provider error classification, account quota, network, and retry logs; do not blindly repeat high-cost tasks. |

## Working Directory, Files, and Tools

| Symptom | Troubleshooting Steps |
| --- | --- |
| Agent cannot find files | Confirm `--working-folder` and relative paths; check that the directory is the expected project. |
| Tool denied | Check permission prompts, IDE permission UI, out-of-project paths, shell commands, and MCP policies. |
| Wrong modification location | Check Git status/diff; prefer `--worktree` for modification tasks. |
| MCP unavailable | Run `anureo mcp list`; check config paths, service commands/URLs, network, and credentials. |

## Sessions and Context

| Symptom | Troubleshooting Steps |
| --- | --- |
| Session not resumed | Confirm the same `--session-id` and project directory are used; check with `anureo session show`. |
| Context seems wrong | Check loaded memory/skills, project scope, and whether the session belongs to another project; disable or delete relevant items if needed. |
| Review/usage failure | The main task should not be blocked; check logs and storage permissions, then retry management commands. |

## ACP / IDE

- Run `anureo acp` standalone in a terminal to confirm it does not write plain text to stdout.
- Check that the IDE's configured executable path, working directory, and model config match the terminal.
- Verify the IDE supports the required ACP capabilities (session resume, permissions, file/terminal, MCP).
- Use `anureo acp --show-log-dir` to locate logs; see the [ACP Guide](acp-ide.md) for more.

## Workflows

1. Confirm whether the status from `workflow_status` is running, failed, or cancelled.
2. For failures, query only the necessary filtered events such as `agent_done`, `run_done`.
3. Check working-directory locks, concurrency settings, model limits, and script input.
4. Resume only when checkpoint and runtime explicitly support it; otherwise fix and restart.

See the [Workflow Guide](workflows.md) for details.

## Still Stuck

Prepare a minimal reproducible project/command, sanitized logs, anureo version, and expected/actual behavior. Rule out local key, private file, and third-party service issues first, then report to maintainers. Use private channels for security issues.
