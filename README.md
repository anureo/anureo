# anureo

anureo is a local-first AI Agent runtime. It lets developers run agents in the CLI, IDE (ACP), and messaging bots, while keeping tool calls, sessions, memory, skills, and workflows within a controllable project context.

anureo's goal is not to replace code review or let agents modify systems unattended, but to enable them to complete real project tasks continuously and interpretably.

> The current version is still evolving. Workflows, browser extension, and task modes include experimental capabilities; `evolve` is not yet implemented.

## Quick Start

### Install a Release Binary

Linux (x86_64):

```sh
curl -fsSL https://raw.githubusercontent.com/anureo/anureo/dev/scripts/install.sh | sh
```

macOS (Intel / Apple Silicon):

```sh
curl -fsSL https://raw.githubusercontent.com/anureo/anureo/dev/scripts/install.sh | sh
```

The macOS installer detects Intel versus Apple Silicon automatically. To install a specific release on Linux or macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/anureo/anureo/dev/scripts/install.sh | sh -s -- --version VERSION
```

To try the latest beta (pre-release) instead:

```sh
curl -fsSL https://raw.githubusercontent.com/anureo/anureo/dev/scripts/install.sh | sh -s -- --beta
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/anureo/anureo/dev/scripts/install.ps1 | iex
```

To install a specific Windows release:

```powershell
$env:ANUREO_VERSION = "VERSION"; irm https://raw.githubusercontent.com/anureo/anureo/dev/scripts/install.ps1 | iex
```

To install the latest Windows beta release:

```powershell
$env:ANUREO_BETA = "1"; irm https://raw.githubusercontent.com/anureo/anureo/dev/scripts/install.ps1 | iex
```

The installers use a user-level directory and do not require administrator privileges. Set `ANUREO_VERSION`, `ANUREO_REPO`, or `ANUREO_INSTALL_DIR` to override the release, repository, or destination. Pass `--beta` (or set `ANUREO_BETA=1`) to install the latest pre-release build (tags like `v0.7.3-beta`); rerun the installer without it to go back to the latest stable.

### 1. Configure Your Model

Copy the example environment file and fill in your model credentials:

```powershell
Copy-Item .env.example .env
```

You can also create a `config.toml` in the user config directory (default `~/.anureo/`, overridable via the `--home DIR` flag). The `.env` in the project root takes precedence over that config.

### 2. Run an Agent in Your Project

```powershell
# Run the default ReAct agent
cargo run -p anureo-cli -- -m "Survey this repo and list test entry points"

# Explicitly specify the agent's working directory
cargo run -p anureo-cli -- --working-folder . "Find failing tests and explain why"

# Continue in the same session
cargo run -p anureo-cli -- --session-id bug-123 "Now fix it and run the relevant tests"
```

Before the first run, verify the agent's effective working directory, model, and tool permissions. For modification tasks, use `--worktree` to run in an isolated Git worktree.

## What anureo Can Do

| Capability | Use Case |
| --- | --- |
| Local Agents | Complete multi-step tasks using ReAct, DUP, ToT, or GoT. |
| Models & Tools | Configure multiple providers, model tiers, MCP, file, shell, web, and more. |
| Persistent Context | Continue project work across sessions with checkpoints, memory, and skills. |
| Workflows | Orchestrate multi-agent tasks in Lua; inspect instance summaries, events, cancel and resume. |
| Multiple Entry Points | Use from CLI, or connect via ACP to compatible IDEs; also supports Telegram multi-bot. |

## Common Commands

```text
anureo -m "task"                         # Start a one-shot task
anureo -i -m "task"                      # Enter an interactive session
anureo --session-id <id> "continue task" # Resume a session
anureo session list                       # List sessions
anureo models                             # List available models
anureo tool list                          # List tools
anureo mcp list                           # Manage MCP services
anureo skills list / anureo memory list     # Manage reusable context
anureo acp                                # Start as an ACP server
```

For full usage, see the [CLI Guide](docs/guides/cli.md).

## Documentation

- [CLI Guide](docs/guides/cli.md)
- [IDE / ACP Integration](docs/guides/acp-ide.md)
- [Workflow Guide](docs/guides/workflows.md)
- [Security & Privacy](docs/guides/security-and-privacy.md)
- [Troubleshooting](docs/guides/troubleshooting.md)

## Development

```powershell
cargo build -p anureo-cli
cargo test -p anureo-cli
```

anureo is a Rust workspace; crate and experimental module details can be found in each module's `Cargo.toml`, source code, and `docs/design/`. User experience and scope are defined by the guides under `docs/`.

## License

MIT
