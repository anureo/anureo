# config

Load configuration from `~/.anureo/config.toml` and optional project `.env`, then apply it to the process environment. Single place for all env/config used by anureo and related tools.

## Priority

Variables are applied only when **not already set** in the process. Precedence (highest first):

1. **Existing environment** — already set in the process
2. **Project `.env`** — from current directory or `override_dir`
3. **anureo config** — `~/.anureo/config.toml` `[env]` table

## Usage

```rust
use config::load_and_apply;

// Load from ~/.anureo/config.toml and optional .env in current dir
load_and_apply("anureo", None)?;

// Load .env from a specific directory instead of current dir
load_and_apply("anureo", Some(project_root.as_path()))?;
```

After calling `load_and_apply`, use `std::env::var("KEY")` as usual; no API changes in the rest of the app.

## Config file

Location: `~/.anureo/config.toml` (override with `$ANUREO_HOME`).

Minimal example:

```toml
[env]
OPENAI_API_KEY = "sk-..."
OPENAI_BASE_URL = "https://api.openai.com/v1"
RUST_LOG = "info"
```

A full example (keys aligned with graphweave `.env`) is in `config/examples/config.toml.example`. Copy it to `~/.anureo/config.toml` and fill in your values.

Only the `[env]` table is read; keys are injected as environment variables when not already set.

## Project .env

A `.env` file in the project directory (or `override_dir`) is supported. Format: `KEY=VALUE` per line, with `#` comments and optional double/single quotes. Missing file is ignored (empty map).

## Errors

`load_and_apply` returns `Result<(), config::LoadError>` for config path, read, or TOML parse failures. Missing config file is not an error (treated as empty).
