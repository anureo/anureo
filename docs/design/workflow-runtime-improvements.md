# Workflow Runtime Improvements

## Purpose

Make anureo workflows predictable for long-running, multi-agent tasks. A workflow
must validate invalid plans before spending tokens, emit a complete terminal
event sequence, preserve enough state to resume safely, and make failures
actionable without manually inspecting JSONL event logs.

## 1. Define and Validate the DSL Contract

`parallel` needs one unambiguous contract. Prefer a mapper form that converts
input items into complete agent configurations:

```lua
parallel(items, function(item)
  return {
    name = item.id,
    prompt = "Implement " .. item.id,
    timeout = 240,
  }
end)
```

Alternatively, support only a list of complete agent configurations. Do not
silently accept both shapes with ambiguous interpretation.

Before starting any agent, load and validate the workflow graph:

- Every agent configuration has a non-empty `prompt`.
- Phase and agent names are unique within their scopes.
- Timeouts are positive and within supported limits.
- A `parallel` mapper returns an agent configuration table for every item.
- Unsupported fields and function signatures fail validation.
- Validation errors identify the phase, item id or index, and Lua source
  location.

This turns an error such as `agent: missing required 'prompt' field` into a
zero-token workflow validation failure instead of a runtime failure after a
phase has already started.

## 2. Require Terminal Event Closure

For every started agent, the event stream must be closed exactly once:

```text
agent_started(agent_id)
  -> agent_progress*
  -> exactly one agent_done(agent_id)
run_done
```

If an agent times out, is cancelled, panics, fails to join, encounters a Lua
callback error, or is interrupted by workflow shutdown, the runtime must append
an `agent_done` event with `status: Error` before writing `run_done`.

The error event should include the failure class, message, workflow phase,
whether retry is appropriate, and the original agent id. For example:

```json
{
  "status": "Error",
  "error_kind": "agent_timeout",
  "message": "agent exceeded 240 seconds",
  "phase": "mvp-mock-browser",
  "retryable": true
}
```

`run_done` must reference the failing agent and include a concise failure
summary. Consumers should never need to infer a failure from a missing terminal
event.

## 3. Use Structured Failure Kinds

Expose a stable machine-readable failure kind on both terminal agent events and
the final run result:

| Failure kind | Meaning | Default handling |
| --- | --- | --- |
| `workflow_validation` | Invalid DSL or agent configuration | Fix the workflow; do not retry |
| `lua_runtime_error` | Lua evaluation failed | Fix the workflow; do not retry |
| `agent_provider_error` | Model, authentication, or rate-limit failure | Retry according to provider policy |
| `agent_timeout` | Agent exceeded its timeout | Retry or split the task |
| `agent_join_error` | Task panic or interrupted join | Retry with diagnostics retained |
| `tool_error` | Command, filesystem, or network tool failed | Retry according to tool semantics |
| `cancelled` | User or parent workflow cancelled execution | Do not auto-retry |
| `verification_failed` | The requested test or validation failed | Fix implementation and resume |

This separates invalid workflows, provider configuration failures, and genuine
application test failures.

## 4. Checkpoint Per Work Item and Resume Safely

The checkpoint should retain state per agent, not only the current phase. Each
completed item records its input hash, outputs, verification result, and status:

```json
{
  "phase": "mvp-mock-browser",
  "items": {
    "fixture-auth": {
      "status": "succeeded",
      "prompt_hash": "...",
      "outputs": ["e2e/fixtures/auth.ts"],
      "verification": "passed"
    },
    "errors-spec": {
      "status": "failed",
      "error_kind": "agent_timeout"
    }
  }
}
```

Provide explicit recovery modes:

- `resume`: continue incomplete or failed items.
- `retry <agent-id>`: retry one item.
- `rerun-phase`: intentionally rerun a whole phase.
- Invalidate completed items when their prompt, declared inputs, or dependency
  outputs change.

## 5. Declare Outputs and Verification

An agent should declare what it owns and how success is checked:

```lua
agent({
  name = "fixture-auth",
  prompt = "...",
  outputs = { "e2e/fixtures/auth.ts" },
  verify = { "npx tsc --noEmit -p e2e/tsconfig.json" },
})
```

On resume, existing outputs that pass their verification can be recorded as
`skipped_cached`. Outputs that fail verification are regenerated. This prevents
agents from repeatedly spending tokens to rediscover that a file already exists.

## 6. Control Concurrency and Workspace Ownership

Only run agents concurrently when their declared write sets do not overlap:

```lua
agent({
  name = "smoke-spec",
  writes = { "e2e/tests/web/smoke.spec.ts" },
})
```

Configuration files, dependency installation, global caches, and shared indexes
should remain serial. The runtime should hold a per-workspace run lock, or
require an explicit override, so multiple workflow processes cannot write the
same project simultaneously.

## 7. Bound Context and Output

Long workflows should not send full reference documents to every sub-agent or
require a full report from every one. The runtime and workflow style should
support:

- Targeted source ranges or acceptance-case ids as context.
- A short terminal summary: changed files, verification command, result.
- Output token budgets and early truncation diagnostics.
- Automatic decomposition of oversized work into independently verifiable
  file-level tasks.
- Phase-level safety checks rather than repeating the same repository report
  for every agent.

## 8. Test Strategy

Cover the runtime in four layers:

1. DSL unit tests for `parallel`, mapper return validation, required fields, and
   source locations.
2. Event-sequence property tests that assert every `agent_started` has exactly
   one terminal `agent_done`, and `run_done` never leaves an agent open.
3. Mock-agent integration tests for success, provider error, timeout,
   cancellation, panic, join failure, and concurrent failures.
4. Persistence end-to-end tests for SQLite checkpointing, CLI failure summaries,
   process restart, and resume.

## Required Invariants

1. Every started agent has one and only one terminal event.
2. Every failed run has a machine-readable, user-readable, and recoverable
   failure reason.
3. No invalid agent configuration launches an agent or consumes model tokens.
4. A resumed run does not rerun verified, unchanged work unless explicitly
   requested.
