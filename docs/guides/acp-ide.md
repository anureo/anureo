# anureo & IDE: ACP Integration Guide

ACP (Agent Client Protocol) lets compatible IDEs use anureo as an Agent server. The CLI is the baseline entry point for capabilities and scripting; ACP surfaces the same project, session, tool, and model semantics inside the editor.

## Starting the Server

```powershell
anureo acp
```

ACP uses stdout for JSON-RPC transport. Do not output extra config, debug text, or shell banners to stdout; use log files and `anureo acp --show-log-dir` for troubleshooting.

Configure the command as `anureo acp` in your IDE. The specific config UI depends on the IDE; ensure it can locate the same anureo binary and model configuration before connecting.

## Sessions and Projects

- The ACP `session_id` created by the IDE maps one-to-one to a anureo thread.
- The absolute working directory passed by the IDE becomes the `working_folder` for that run.
- Loading a session restores the working directory and persisted model, mode, effort, and other session config.
- Forking a session creates an independent successor; subsequent conversations and config should not write back to the source session.

When switching projects, create or explicitly load the corresponding session. Do not reuse the same session across different projects without indication.

## Permissions, Files, and Terminals

ACP clients can declare file, terminal, and MCP capabilities. anureo requests confirmation via `session/request_permission` when needed; if the client denies or cancels, the tool call ends as denied/cancelled, not as success.

When approving requests, check the tool, target path or command, working directory, and authorization scope. "Allow once" applies only to the current call; long-term or project-level authorization should be managed through explicit, reviewable IDE/project configuration.

## Streaming Status and Cancellation

The IDE receives session updates, tool-call status, and the final stop reason. `session/cancel` cancels the current run; the terminal state after cancellation should be `cancelled`, not `success`. If the editor does not show the full error, check its ACP log and the anureo log.

## Common Issues

| Symptom | Check |
| --- | --- |
| IDE cannot connect | Run `anureo acp` in a terminal; verify the binary, JSON-RPC stdout, and log path. |
| Model mismatch | Check project `.env`, `~/.anureo/config.toml`, and session-level config. |
| Tool denied | Check the IDE's permission UI, working directory, and MCP declarations. |
| Session not restored | Confirm the session ID and working directory are unchanged; check checkpoint/session storage. |
