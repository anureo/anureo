# Changelog

All notable changes to `telegram-bot` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.5] — 2025-08-19

### Added

- Multi-bot long polling framework (`bot.rs`)
- Streaming agent responses with Think/Act phases (`streaming/`)
- Model selection with SQLite-backed catalog (`model_selection.rs`)
- Slash command system: `/model`, `/reset`, `/help` (`command/`)
- File download for photos, videos, documents (`download.rs`)
- Health check HTTP server with axum (`health.rs`)
- Prometheus-style atomic metrics (`metrics.rs`)
- Message formatting: MarkdownV2 + HTML with fallback (`formatting/`)
- Dependency injection via `traits.rs` + `handler_deps.rs`
- Retry mechanism for Telegram API calls (`streaming/retry.rs`)
- Configuration from `~/.anureo/telegram-bot.toml` with `${ENV}` interpolation
- Streaming message handler with throttled edits (`streaming/message_handler.rs`)

### Changed

- Refactored message routing from monolithic handler to router → pipeline → streaming

### Known Issues

- 8 `unwrap()` calls in `retry.rs` critical path (potential panic under network instability)
- `download.rs` lacks path traversal validation
- `retry.rs` uses fixed 1s delay without exponential backoff
- Compilation blocked by upstream `ToolCallContent.len()` error in `anureo` crate
