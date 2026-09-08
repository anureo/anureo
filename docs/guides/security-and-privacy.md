# anureo Security & Privacy

anureo can read and write files, run commands, connect to MCP services, operate browsers, and store session and workflow data. Treat it as a privileged development tool, not a risk-free chat window.

## Four Checks Before Use

1. **Working directory**: Confirm `--working-folder` is the intended project; out-of-project paths need extra review.
2. **Tool impact**: File writes, shell, network/MCP, browser submissions, and delete operations can all cause external side effects.
3. **Credentials**: Put keys in environment variables or an uncommitted `.env`; never in prompts, skills, memory, logs, or workflow source.
4. **Result verification**: An agent's `completed` only means the run finished; it does not mean the code, data, or business conclusions are correct.

## Authorization Principles

For high-impact calls, confirmation prompts should include the tool, action, target scope, and authorization duration. Prefer one-time authorization; choose session or project-level authorization only when you understand the risk.

| Operation | Recommendation |
| --- | --- |
| Read project files | Confirm the project directory is correct; note that files may contain sensitive config. |
| Modify files | Review diffs; use `--worktree` for high-risk changes. |
| Shell/install commands | Check cwd, network access, deletion, and publishing side effects. |
| MCP services | Enable only trusted services; review their commands, URLs, headers, and environment variables. |
| Browser forms/uploads | Confirm the site, submit action, uploaded files, and login state. |
| Delete/publish | Must be explicitly confirmed each time; do not rely on "default allow forever." |

ACP interacts through the IDE's permission requests; CLI or non-interactive runs should use explicit policies. When no confirmation mechanism is available, high-risk operations should not execute silently.

## Data Storage and Cleanup

anureo uses the user config directory and project `.anureo/` to store configuration, session associations, memory, skills, and workflow instances.

- Before deleting sessions, memory, skills, or instances, confirm the specific target; these operations may not be reversible.
- Logs may contain operational metadata; keep them in controlled local locations with rotation.
- Only export user-selected objects; review exports for prompts, outputs, paths, or internal reports before sharing.
- Code, prompts, tool outputs, or paths should not be uploaded as telemetry by default. Any upload must be explicitly opt-in.

## Git Worktree Isolation

For modification tasks:

```powershell
anureo --worktree -m "Modify and test this feature"
```

anureo runs in an isolated worktree; it cleans up when there are no changes, and preserves the directory for review when there are. Inspect retained branches, diffs, and untracked files before deciding to merge or delete. Cancelling an agent does not automatically undo all completed external operations.

## When Issues Arise

Immediately stop high-risk runs, save relevant session/instance IDs and log locations, revoke or rotate exposed credentials, and use version control or backups to check the scope of changes. Do not disclose sensitive details about security flaws through regular prompts; use the private reporting channel designated by the project maintainers.
