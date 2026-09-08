# Workflow Development Guide

Workflows orchestrate multi-agent tasks using Lua scripts. Each run produces a saved **instance** with events, checkpoints, reports, and source snapshots, making long-running and parallel agent tasks observable, resumable, and auditable.

---

## 1. Overview

### 1.1 What is a Workflow

A workflow is a Lua script that orchestrates one or more agents. Unlike a single agent conversation, a workflow can:

- Run multiple agents in parallel (`parallel`)
- Chain agents through multi-stage pipelines (`pipeline`)
- Iterate in loops with conditional branching
- Resume from failures and interruptions
- Produce structured reports

### 1.2 When to Use a Workflow

| Use case | Single agent | Workflow |
|---|---|---|
| One-shot question answering | ✅ | ❌ |
| Multi-file refactoring with validation | ❌ | ✅ |
| Parallel research across sources | ❌ | ✅ |
| Goal-driven iterative coding loop | ❌ | ✅ |
| Adversarial verification (voting) | ❌ | ✅ |

### 1.3 Architecture

```
┌─────────────────────────────────────────────────┐
│                   Workflow Tool                  │
│  workflow_start / workflow_status / workflow_events             │
│  workflow_cancel / workflow_list / workflow_source              │
└───────────────────────┬─────────────────────────┘
                        │
┌───────────────────────▼─────────────────────────┐
│              WorkflowRuntime                     │
│  - Instance path resolution                      │
│  - Active run registry (cancellation)            │
│  - Finalize → instance.json                     │
└───────────────────────┬─────────────────────────┘
                        │
┌───────────────────────▼─────────────────────────┐
│              Luft Engine (v0.4)                   │
│  - Lua sandbox execution                         │
│  - DSL: agent(), parallel(), pipeline(), etc.     │
│  - Checkpoint, events, resume                    │
└───────────┬───────────────────┬──────────────────┘
            │                   │
┌───────────▼───────────┐ ┌─────▼──────────────────┐
│   anureoAgentBackend     │ │   On-disk Instance     │
│  - AgentConfig setup   │ │  .anureo/instances/<id>/ │
│  - Schema injection    │ │   ├── instance.json    │
│  - Tool filtering      │ │   ├── checkpoint.json  │
│  - Token tracking      │ │   ├── events.jsonl     │
│  - Output finalization │ │   ├── workflow.lua     │
└───────────────────────┘ │   └── agent-outputs/    │
                          └─────────────────────────┘
```

The **Luft engine** executes Lua scripts and provides the DSL. The **anureoAgentBackend** bridges Luft's `AgentBackend` trait to anureo's agent execution. The **WorkflowRuntime** manages instance paths, cancellation, and finalization.

---

## 2. Lua DSL Reference

### 2.1 `agent(opts)` → result

Run one sub-agent to completion. This is the fundamental building block.

**opts:**

| field | type | required | description |
|---|---|---|---|
| `prompt` | string | yes | Instructions for the sub-agent |
| `schema` | table | no | JSON Schema constraining structured output |
| `model` | string | no | Override the model for this agent |
| `name` | string | no | Short identifier for CLI display |
| `description` | string | no | One-line CLI description |
| `timeout_ms` | int | no | Per-agent timeout |
| `working_folder` | string | no | Override working folder for this agent |

**result:**

| field | type | description |
|---|---|---|
| `ok` | bool | True if the agent succeeded |
| `status` | string | `"ok"` / `"error"` / `"cancelled"` / `"timed_out"` |
| `output` | table | Parsed agent response (Lua table) |
| `tokens` | int | Token usage |
| `findings` | array? | Accumulated findings (when applicable) |

**Schema rules:**

- Always check `result.ok` before using `result.output`.
- **Analysis agents** (extract / analyze / verify): MUST provide a schema. JSON-mode lets the orchestrator consume structured fields reliably.
- **Execution agents** (write / edit / refactor): omit the schema or use a minimal one like `{changed=bool, files=string[]}`. A rich schema forces JSON-mode and **prevents tool calls**, which execution agents need.
- Put concrete context in `prompt`: file paths, module names, acceptance criteria.

```lua
local r = agent({
  name = "code-reviewer",
  prompt = "Review src/auth.rs for security issues",
  schema = {
    type = "object",
    properties = {
      issues = { type = "array", items = { type = "string" } },
      severity = { type = "string", enum = { "low", "medium", "high" } },
    },
    required = { "issues", "severity" },
  },
})
if not r.ok then
  log("review failed: " .. r.status, "error")
  return
end
-- r.output is a structured table: { issues = [...], severity = "high" }
```

### 2.2 `parallel(items, mapFn)` → result[]

**Barrier fan-out**: runs all items concurrently, waits for **all** to finish.

```lua
local results = parallel(items, function(item)
  return { prompt = "Process: " .. item.id, schema = ITEM_SCHEMA }
end)
-- results[i] corresponds to items[i]
```

Use when you need every result before continuing. Max ~16 concurrent agents.

### 2.3 `pipeline{ items, stages, max_inflight }` → `{ items, ok, failed }`

**Streaming multi-stage**: items flow through all stages; different items can be in different stages simultaneously.

```lua
local out = pipeline {
  items = urls,
  max_inflight = 4,
  stages = {
    function(item)
      local r = agent({ prompt = "Fetch " .. item.url })
      return { url = item.url, body = r.output.body, ok = r.ok }
    end,
    function(prev)
      if not prev.ok then return prev end   -- error degradation
      local r = agent({ prompt = "Summarize: " .. prev.body, schema = SUM_SCHEMA })
      return { url = prev.url, summary = r.output.summary, ok = r.ok }
    end,
  },
}
```

Each stage handler MUST call `agent()` itself. Stage 1 receives the raw item; later stages receive the previous stage's return value. Error degradation: check `prev.ok` at handler start; on failure, return default data directly (skip the agent call).

### 2.4 `phase(name, planned?)` → phase_id

Declares a progress phase. Emits a CLI-visible event.

```lua
phase("analyze", 8)    -- named "analyze", ~8 units of work planned
```

### 2.5 `phase_begin(name)` / `phase_end(span)`

Span-based phase timing. Must be paired. Alternative to `phase()` when you need precise timing.

### 2.6 `report(value)`

Sets the final output. **First call wins**; later calls are silently ignored. Always `return` after an error report:

```lua
if not r.ok then
  report({ error = "phase X failed: " .. r.status })
  return
end
```

### 2.7 `log(msg, level?)`

Log a message. `level`: `"info"` (default) / `"warn"` / `"error"`.

```lua
log("agent completed with " .. tostring(r.tokens) .. " tokens", "info")
```

### 2.8 `budget(time_ms?, max_rounds?)`

Hint resource limits for the current phase. Soft limits — the engine may exceed.

### 2.9 `workflow(path, args?)` → result

Calls another saved `.lua` workflow as a sub-step. Use to compose larger workflows from smaller ones.

```lua
local audit = workflow(".anureo/workflows/goal-audit.lua", {
  objective = ctx.objective,
  coding_output = coding_result.output,
  history = ctx.history,
})
```

### 2.10 `json.encode(value)` / `json.decode(string)`

Round-trip between Lua tables and JSON strings.

```lua
local encoded = json.encode({ name = "test", count = 42 })
local decoded = json.decode(encoded)
```

### 2.11 Globals

| global | type | description |
|---|---|---|
| `args` | table | User-supplied arguments (from the tool's `args` field) |
| `ctx` | table | Run context; `ctx.run_id` is the current run ID (string) |

---

## 3. Workflow Lifecycle

### 3.1 Start

`workflow_start` accepts three mutually exclusive modes:

- **`script`**: inline Lua source
- **`workflow`**: path to a `.lua` file under `.anureo/workflows/`
- **`resume_from_id`**: instance identifier to resume a prior run

```lua
-- From file
workflow_start({
  workflow = ".anureo/workflows/audit.lua",
  args = { target = "src/auth.rs" },
})

-- Inline script
workflow_start({
  script = [[ function main() report({ msg = "hello" }) end ]],
})
```

The engine:
1. Resolves the workflow source (file or inline)
2. Injects `args` as a Lua global
3. Creates an instance directory under `.anureo/instances/<id>/`
4. Builds a Luft engine with the `anureoAgentBackend`
5. Returns `{ instance_dir, status: "running" }` immediately
6. Spawns a background task for finalization

### 3.2 Execution

The Lua `main()` function runs in a sandboxed environment. The Luft engine:

- Provides the DSL functions as Lua globals
- Maintains a checkpoint file with current state
- Writes events to `events.jsonl` as agents run
- Supports cancellation via `CancellationToken`

### 3.3 Observation

Poll `workflow_status` with the `instance_dir` to follow progress:

```text
workflow_start → record instance_dir
→ workflow_status(instance_dir)  [poll while status is "running"]
```

For detailed event inspection, use `workflow_events` with type filters:

```text
workflow_events(instance_dir, types=["agent_done", "run_done"])
```

### 3.4 Cancellation

`workflow_cancel` signals the running instance. The in-flight agent finishes its current turn (or is interrupted), then the checkpoint is marked `cancelled`. Poll `workflow_status` to observe the terminal state.

### 3.5 Finalization

When the workflow reaches a terminal state (completed / failed / cancelled), the background finalization task:

1. Reads the final checkpoint
2. Builds `instance.json` with summary metadata
3. Writes agent output files for large outputs
4. Unregisters the run from the active registry

---

## 4. Instance Model

### 4.1 Directory Structure

```
.anureo/instances/<instance-dir>/
├── instance.json       # Final summary (after terminal state)
├── checkpoint.json     # Runtime state (checkpoint during execution)
├── events.jsonl        # Event stream (one JSON object per line)
├── workflow.lua        # Source snapshot of the executed script
└── agent-outputs/      # Large agent outputs (written if > inline limit)
    ├── <agent-id-1>.txt
    └── <agent-id-2>.txt
```

### 4.2 instance.json

Contains the terminal summary: status, agents, phases, tokens, elapsed time, event stats, report preview, and workflow reference. Sensitive fields (absolute paths, output refs, checkpoint hash) are sanitized before returning to the caller.

### 4.3 Event Stream (`events.jsonl`)

Every event is a JSON object on a single line. The event sequence for a healthy run:

```text
agent_started(agent_id)
  → agent_progress*  (optional, streaming updates)
  → exactly one agent_done(agent_id)
run_done
```

Events support filtering by `type`, `agent_id`, and pagination via `offset` + `events_limit`.

### 4.4 Checkpoint (`checkpoint.json`)

Maintained by the Luft engine during execution. Contains run state, agent results, and status. Used for resume — the engine reads the checkpoint to determine which agents completed and which need re-execution.

---

## 5. Writing Workflows: Best Practices

### 5.1 Pattern 1: Static Decomposition

Hardcoded list of items, each processed in sequence. Simple and predictable.

```lua
meta = {
  reasoning = "Analyze, refactor, verify each module in sequence",
  phases = {
    { label = "analyze", description = "Analyze each module", agents = 3 },
    { label = "refactor", description = "Apply refactoring", agents = 3 },
    { label = "verify", description = "Verify tests pass", agents = 3 },
  },
}

local MODULES = { "auth", "db", "api" }

function main()
  local results = {}
  for _, mod in ipairs(MODULES) do
    phase(mod)
    local a = agent({ prompt = "Analyze " .. mod, schema = ANALYSIS_SCHEMA })
    if not a.ok then log("failed", "warn"); goto continue end
    local c = agent({ prompt = "Refactor " .. mod, schema = CHANGES_SCHEMA })
    if not c.ok then log("failed", "warn"); goto continue end
    local v = agent({ prompt = "Verify " .. mod, schema = VERIFY_SCHEMA })
    table.insert(results, { module = mod, passed = v.ok })
    ::continue::
  end
  report({ results = results })
end
```

### 5.2 Pattern 2: Dynamic Enumeration

The number of items isn't known upfront — an agent discovers them first.

```lua
function main()
  phase("discover")
  local discover = agent({
    prompt = "List subsystems under src/ that need refactoring",
    schema = SUBSYSTEMS_SCHEMA,
  })
  if not discover.ok then report({ error = "discovery failed" }); return end

  local results = {}
  for _, sys in ipairs(discover.output.subsystems or {}) do
    phase(sys.name)
    -- Discover modules within this subsystem
    local mods = agent({ prompt = "List modules in " .. sys.path, schema = MODULES_SCHEMA })
    for _, mod in ipairs(mods.output.modules or {}) do
      -- Process each module: analyze → change → verify
      local a = agent({ prompt = "Analyze " .. mod.path, schema = ANALYSIS_SCHEMA })
      local c = agent({ prompt = "Apply to " .. mod.path, schema = CHANGES_SCHEMA })
      local v = agent({ prompt = "Verify " .. mod.path, schema = VERIFY_SCHEMA })
      table.insert(results, { module = mod.name, ok = v.ok })
    end
  end
  report({ refactored = #results, results = results })
end
```

### 5.3 Pattern 3: Adversarial Verification

Multiple voters cross-check findings. Keep findings with approval rate ≥ threshold.

```lua
function main()
  phase("gather")
  local gather = agent({
    prompt = "List key findings to verify",
    schema = FINDINGS_SCHEMA,
  })
  if not gather.ok then report({ error = "gather failed" }); return end

  local items = gather.output.findings or {}
  local threshold = 0.7
  local voters = 3

  for round = 1, 3 do
    phase("vote round " .. round)
    local all_votes = parallel(items, function(finding)
      return {
        prompt = "Evaluate this finding: " .. json.encode(finding),
        schema = VOTE_SCHEMA,
      }
    end)
    -- Filter: keep findings with approval rate >= threshold
    local survivors = {}
    for i, finding in ipairs(items) do
      local approved = 0
      for j = 1, voters do
        if all_votes[(i-1)*voters + j].output.approve then
          approved = approved + 1
        end
      end
      if approved / voters >= threshold then
        table.insert(survivors, finding)
      end
    end
    if #survivors == #items then break end  -- converged
    items = survivors
  end
  report({ survivors = #items, findings = items })
end
```

### 5.4 Pattern 4: Iterative Goal Loop

For goal-driven tasks that need multiple iterations of coding + audit + steering. See `.anureo/workflows/goal-run.lua` for the full implementation.

```lua
function main()
  local max_iterations = 100
  local status = "active"
  local iteration = 0

  while iteration < max_iterations and status == "active" do
    iteration = iteration + 1
    phase("iteration " .. iteration)

    -- Execute coding agent
    local coding = agent({
      name = "coding-agent",
      prompt = "Continue working toward: " .. args.objective,
    })
    if not coding.ok then status = "failed"; break end

    -- Audit: check if complete or blocked
    phase("audit")
    local audit = agent({
      prompt = "Is the objective complete?",
      schema = AUDIT_SCHEMA,
    })
    if audit.output.status == "complete" then
      status = "complete"; break
    end
  end

  report({
    ok = true,
    status = status,
    iterations = iteration,
  })
end
```

### 5.5 Error Handling

- ALWAYS check `result.ok` before using `result.output`.
- On failure: `log()` the error, then decide — skip, retry, or abort with `report()`.
- Always `return` after an error `report()` to prevent nil dereference.
- Graceful degradation: when a stage fails, feed a minimal/default prompt to the next stage rather than crashing the pipeline.

```lua
if not r.ok then
  log("agent failed: " .. r.status, "error")
  report({ error = "phase X failed" })
  return
end
```

### 5.6 Schema Usage Rules

| Agent type | Schema | Why |
|---|---|---|
| Analysis (extract, analyze, verify) | **MUST provide** | JSON-mode enables reliable structured consumption |
| Execution (write, edit, refactor) | **omit or minimal** | Rich schema prevents tool calls |

### 5.7 Constraints

- Scripts orchestrate only — no filesystem / shell access from the Lua script itself. Use agents to perform file operations.
- `report()` exactly once per workflow run.
- Fan-out bounded: max ~16 concurrent agents via `parallel()`.
- Workflow nesting depth: max 3 levels.
- Double-quote all string values, especially non-ASCII text — Lua syntax requires it.

---

## 6. Debugging and Diagnostics

### 6.1 Troubleshooting Order

```text
workflow_list(status_filter="failed")
→ workflow_status(instance_dir)
→ workflow_events(instance_dir, types=["agent_done", "run_done"])
→ workflow_source(instance_dir) if needed
```

### 6.2 Failure Kinds

| Failure kind | Meaning | Default handling |
|---|---|---|
| `workflow_validation` | Invalid DSL or agent configuration | Fix the workflow; do not retry |
| `lua_runtime_error` | Lua evaluation failed | Fix the workflow; do not retry |
| `agent_provider_error` | Model, authentication, or rate-limit failure | Retry according to provider policy |
| `agent_timeout` | Agent exceeded its timeout | Retry or split the task |
| `agent_join_error` | Task panic or interrupted join | Retry with diagnostics retained |
| `tool_error` | Command, filesystem, or network tool failed | Retry according to tool semantics |
| `cancelled` | User or parent workflow cancelled execution | Do not auto-retry |
| `verification_failed` | The requested test or validation failed | Fix implementation and resume |

### 6.3 Event Filtering

`workflow_events` supports:

- `types`: filter by event type (e.g., `["agent_done", "run_done"]`)
- `agent_id`: filter by specific agent
- `offset` + `events_limit`: pagination

Always start from the summary, then narrow the event scope as needed.

---

## 7. Tool Reference

### 7.1 `workflow_start`

| field | type | required | description |
|---|---|---|---|
| `script` | string | conditional | Inline Lua source |
| `workflow` | string | conditional | Path to `.lua` file |
| `resume_from_id` | string | conditional | Resume a prior run |
| `args` | table | no | Arguments exposed as `args` global |
| `concurrency` | int | no | Max concurrent agents (1..64, default 4) |

Exactly one of `script`, `workflow`, or `resume_from_id` must be provided.

Returns: `{ instance_dir, status: "running" }`

### 7.2 `workflow_status`

| field | type | required | description |
|---|---|---|---|
| `instance` | string | yes | Instance directory name |

Returns: Instance summary (status, agents, phases, tokens, report preview).

### 7.3 `workflow_events`

| field | type | required | description |
|---|---|---|---|
| `instance` | string | yes | Instance directory name |
| `types` | string[] | no | Event type filter |
| `agent_id` | string | no | Agent ID filter |
| `offset` | int | no | Skip N matching events |
| `events_limit` | int | no | Page size (1..500, default 50) |

Returns: `{ instance_dir, offset, events_limit, total_matching, next_offset, events }`

### 7.4 `workflow_cancel`

| field | type | required | description |
|---|---|---|---|
| `instance` | string | yes | Instance directory name |
| `instance_dir` | string | yes | Alias for `instance` |

Returns: `{ instance_dir, result: "cancelling" | "not_found_or_terminal" }`

### 7.5 `workflow_list`

| field | type | required | description |
|---|---|---|---|
| `limit` | int | no | Max results (1..100, default 20) |
| `cursor` | string | no | Pagination cursor |
| `status_filter` | string | no | Filter by status (completed/failed/cancelled) |

Returns: `{ instances, count, next_cursor, has_more }`

### 7.6 `workflow_source`

| field | type | required | description |
|---|---|---|---|
| `instance` | string | yes | Instance directory name |

Returns: `{ instance_dir, workflow_source, truncated }`

### 7.7 `workflow_files`

No arguments. Returns list of available `.lua` workflow files under `.anureo/workflows/`.

---

## Appendix: Complete Examples

### Example A: Per-module Refactoring (Static Decomposition)

```lua
meta = {
  reasoning = "Decompose by module; analyze, refactor, verify each in sequence",
  phases = {
    { label = "analyze", description = "Analyze each module for issues", agents = 3 },
    { label = "refactor", description = "Apply refactoring to each module", agents = 3 },
    { label = "verify", description = "Verify refactored modules pass tests", agents = 3 },
    { label = "report" },
  },
}

local MODULES = { "auth", "db", "api" }

local ANALYSIS = {
  type = "object",
  properties = {
    issues = { type = "array", items = { type = "string" } },
    summary = { type = "string" },
  },
  required = { "issues", "summary" },
}
local CHANGES = {
  type = "object",
  properties = { changed = { type = "boolean" }, files_modified = { type = "array", items = { type = "string" } } },
  required = { "changed" },
}
local VERIFY = {
  type = "object",
  properties = { passed = { type = "boolean" }, details = { type = "string" } },
  required = { "passed" },
}

function main()
  local results = {}
  for _, mod in ipairs(MODULES) do
    local name = "refactor " .. mod
    phase(name)

    phase("analyze")
    local a = agent({
      prompt = "Review module `" .. mod .. "` under src/. Identify long functions, duplicate logic, missing error handling.",
      schema = ANALYSIS,
    })
    if not a.ok then log("analyze failed", "warn"); goto continue end

    phase("refactor")
    local c = agent({
      prompt = "Apply refactoring to `" .. mod .. "`: " .. json.encode(a.output.issues),
      schema = CHANGES,
    })
    if not c.ok then log("refactor failed", "warn"); goto continue end

    phase("verify")
    local v = agent({
      prompt = "Verify `" .. mod .. "` passes tests after refactoring.",
      schema = VERIFY,
    })
    table.insert(results, { module = mod, ok = v.ok and v.output.passed })
    ::continue::
  end
  report({ refactored = #results, results = results })
end
```

### Example B: Whole-crate Refactoring (Dynamic Enumeration)

```lua
meta = {
  reasoning = "Two-stage discovery: enumerate subsystems, then modules per subsystem",
  phases = {
    { label = "discover subsystems" },
    { label = "discover modules", dynamic = true },
    { label = "analyze", dynamic = true },
    { label = "change", dynamic = true },
    { label = "verify", dynamic = true },
    { label = "report" },
  },
}

local SUBSYSTEMS_SCHEMA = {
  type = "object",
  properties = {
    subsystems = {
      type = "array",
      items = {
        type = "object",
        properties = { name = { type = "string" }, path = { type = "string" } },
        required = { "name", "path" },
      },
    },
  },
  required = { "subsystems" },
}
local MODULES_SCHEMA = {
  type = "object",
  properties = {
    modules = {
      type = "array",
      items = {
        type = "object",
        properties = { name = { type = "string" }, path = { type = "string" } },
        required = { "name", "path" },
      },
    },
  },
  required = { "modules" },
}

function main()
  phase("discover subsystems")
  local discover = agent({
    prompt = "Scan the crate under src/ and list subsystems needing refactoring.",
    schema = SUBSYSTEMS_SCHEMA,
  })
  if not discover.ok then report({ error = "discovery failed" }); return end

  local results = {}
  for _, sys in ipairs(discover.output.subsystems or {}) do
    phase(sys.name)
    local mods = agent({
      prompt = "List modules in `" .. sys.path .. "` that need changes.",
      schema = MODULES_SCHEMA,
    })
    for _, mod in ipairs(mods.output.modules or {}) do
      phase(mod.name)
      phase("analyze")
      local a = agent({ prompt = "Analyze `" .. mod.path .. "`", schema = { type = "object", properties = { summary = { type = "string" } }, required = { "summary" } } })
      if not a.ok then goto next end
      phase("change")
      local c = agent({ prompt = "Apply refactoring to `" .. mod.path .. "`: " .. a.output.summary, schema = { type = "object", properties = { changed = { type = "boolean" } }, required = { "changed" } } })
      phase("verify")
      local v = agent({ prompt = "Verify `" .. mod.path .. "` passes tests.", schema = { type = "object", properties = { passed = { type = "boolean" } }, required = { "passed" } } })
      table.insert(results, { module = mod.name, changed = c.output.changed, passed = v.ok and v.output.passed or false })
      ::next::
    end
  end
  report({ modules_refactored = #results, results = results })
end
```

### Example C: Adversarial Verification (Cross-check via Voting)

```lua
meta = {
  reasoning = "Multi-round adversarial loop: vote on each finding, keep approved, iterate",
  phases = {
    { label = "gather" },
    { label = "vote", dynamic = true },
    { label = "report" },
  },
}

local FINDINGS_SCHEMA = {
  type = "object",
  properties = {
    findings = {
      type = "array",
      items = {
        type = "object",
        properties = { claim = { type = "string" }, evidence = { type = "string" } },
        required = { "claim" },
      },
    },
  },
  required = { "findings" },
}
local VOTE_SCHEMA = {
  type = "object",
  properties = { approve = { type = "boolean" }, reason = { type = "string" } },
  required = { "approve" },
}

function main()
  phase("gather")
  local gather = agent({
    prompt = "List key findings to verify. For each, state the claim and supporting evidence.",
    schema = FINDINGS_SCHEMA,
  })
  if not gather.ok then report({ error = "gather failed" }); return end

  local items = gather.output.findings or {}
  local max_rounds = 3
  local threshold = 0.7
  local voters = 3

  for round = 1, max_rounds do
    phase("vote round " .. round)
    log("round " .. round .. ", " .. #items .. " items")

    local all_votes = parallel(items, function(finding)
      return {
        prompt = "Evaluate this finding: " .. json.encode(finding) .. "\nVote approve=true only if well-supported.",
        schema = VOTE_SCHEMA,
      }
    end)

    local survivors = {}
    for i, finding in ipairs(items) do
      local approved = 0
      for j = 1, voters do
        local v = all_votes[(i - 1) * voters + j]
        if v.ok and v.output.approve then approved = approved + 1 end
      end
      if approved / voters >= threshold then
        table.insert(survivors, finding)
      end
    end

    if #survivors == #items then log("converged"); break end
    items = survivors
  end

  report({ survivors = #items, findings = items })
end
```

### Example D: Goal System (Iterative Loop)

See `.anureo/workflows/goal-run.lua` for the full 367-line implementation. The key pattern is:

```lua
function main()
  -- Initialize goal state
  local goal = { status = "active", ... }
  local ctx = { iteration = 0, ... }

  while ctx.iteration < max_iterations and goal.status == "active" do
    ctx.iteration = ctx.iteration + 1
    phase("continuation")

    -- Execute coding agent toward the objective
    local coding = agent({ name = "coding-agent", prompt = build_prompt(ctx) })
    if not coding.ok then break end

    -- Track tokens, check budget
    ctx.tokens_used = ctx.tokens_used + (coding.tokens or 0)
    if budget_exhausted(ctx.tokens_used, ctx.token_budget) then
      goal.status = "budget_limited"; break
    end

    phase("audit")
    -- Audit: complete, incomplete, or blocked?
    local audit = agent({ prompt = build_audit_prompt(ctx), schema = AUDIT_SCHEMA })
    if audit.output.status == "complete" then goal.status = "complete"; break end
    if audit.output.status == "blocked" and consecutive_blocks >= 3 then
      goal.status = "blocked"; break
    end
  end

  report({ ok = true, goal = goal, summary = { iterations = ctx.iteration, ... } })
end
```

---

## See Also

- `docs/guides/workflows.md` — User guide for starting and observing workflows
- `docs/design/workflow-runtime-improvements.md` — Design decisions on checkpoint, resume, and event closure
- `docs/goal/goal-system-workflow.md` — Goal system design using workflow orchestration