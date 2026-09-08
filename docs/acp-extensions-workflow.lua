-- ============================================================
-- anureo ACP Extension Full Implementation Workflow
-- ============================================================
-- Per-domain pipeline (8 stages):
--   1. Architecture design
--   2. Adversarial architecture review
--   3. Implementation
--   4-5. Code review → fix loop (max 20)
--   6. Test writing
--   7-8. Test review → test fix loop (max 20)
--
-- Phases:
--   A: Quick fixes (fork capability + diagnostic logging)
--   B: Extension framework (registry + dispatch + pre-declare all mods)
--   C: Wave 1 — Core (files/git/worktree/mcp) × 8-stage pipeline
--   D: Compile gate (fix framework issues before mass parallel)
--   E: Wave 2 — All remaining 28 domains × 8-stage pipeline
--   F: Registration wiring + Relay (parallel)
--   G: Final verification (compile + test + capability)
-- ============================================================

meta = {
  reasoning = "Each domain goes through 8 stages: architecture → adversarial arch-review → implementation → code-review→fix loop (×20) → test-writing → test-review→fix loop (×20). "
           .. "Framework is serial prerequisite. Wave 1 validates via compile gate before mass parallel. Registration + Relay parallel.",
  phases = {
    { label = "quick-fix",      description = "Fork capability + diagnostic logging",        agents = 1 },
    { label = "framework",      description = "Extension framework + dispatch + stubs",       agents = 1 },
    { label = "wave1-core",     description = "Core 4 domains × 8-stage pipeline",            agents = 32, dynamic = true },
    { label = "compile-gate",   description = "Compile check + fix framework issues",         agents = 1 },
    { label = "wave-rest",      description = "28 domains × 8-stage pipeline",                agents = 224, dynamic = true },
    { label = "register-relay", description = "Registration + Relay (parallel)",              agents = 2 },
    { label = "verify",         description = "Compile + test + capability check",            agents = 1 },
    { label = "report" },
  },
}

-- ============================================================
-- Domain definitions
-- ============================================================

local DOMAINS = {
  -- ── Wave 1: Core ──────
  { wave = 1, name = "files", spec = "12-files.md", module = "files",
    methods = "list, search, stat, read, write, create, delete, move, copy, rename, tree, changed",
    ref = "apps/acp/src/tools/fs_tools.rs" },
  { wave = 1, name = "git", spec = "11-git.md", module = "git",
    methods = "status, diff, branches, log, blame, stage, unstage, commit, amend, push, pull, fetch, merge, rebase, stash (create/pop/list/apply/drop), cherry_pick, identity (list/add/remove/set_active)",
    ref = "Check if git2 or gix crate is in workspace Cargo.toml. If not, shell out to git CLI." },
  { wave = 1, name = "worktree", spec = "10-worktree.md", module = "worktree",
    methods = "list, get, create, delete, changed",
    ref = "Check apps/server/ for existing worktree management." },
  { wave = 1, name = "mcp", spec = "13-mcp.md", module = "mcp",
    methods = "list, get, status, configure, enable, disable, status_changed",
    ref = "apps/acp/src/mcp_convert.rs for MCP types" },

  -- ── Wave 2: All remaining ──────
  { wave = 2, name = "goal", spec = "14-goal-scheduled-task.md", module = "goal",
    methods = "list, get, start, pause, resume, cancel, changed", ref = "" },
  { wave = 2, name = "scheduled-task", spec = "14-goal-scheduled-task.md", module = "scheduled_task",
    methods = "list, run, cancel, changed", ref = "" },
  { wave = 2, name = "connection", spec = "15-connection-relay-pairing-auth.md", module = "connection",
    methods = "list, get, close", ref = "apps/server/src/acp_hub.rs" },
  { wave = 2, name = "relay", spec = "15-connection-relay-pairing-auth.md", module = "relay",
    methods = "list, get, close", ref = "" },
  { wave = 2, name = "pairing", spec = "15-connection-relay-pairing-auth.md", module = "pairing",
    methods = "create, get, list, revoke, redeem", ref = "" },
  { wave = 2, name = "client-auth", spec = "15-connection-relay-pairing-auth.md", module = "client_auth",
    methods = "list, get, revoke, changed", ref = "" },
  { wave = 2, name = "question", spec = "16-question.md", module = "question",
    methods = "ask, reply, cancel", ref = "Standard ACP session/request_permission" },
  { wave = 2, name = "github", spec = "17-github.md", module = "github",
    methods = "auth_status, authenticate, list_prs, get_pr, create_pr, update_pr, merge_pr, close_pr, review_pr, list_issues, get_issue, create_issue, update_issue, search, list_reviews, create_review, auth_changed",
    ref = "Check if octocrab crate is in workspace" },
  { wave = 2, name = "notification", spec = "18-notification.md", module = "notification",
    methods = "register, unregister, list, update, mark_read, mark_all_read, changed", ref = "" },
  { wave = 2, name = "skills", spec = "20-skills.md", module = "skills",
    methods = "list, get, install, uninstall, enable, disable, changed", ref = "anureo skill system" },
  { wave = 2, name = "session-folder", spec = "21-session-folder.md", module = "session_folder",
    methods = "list, get, create, update, delete, move, changed", ref = "apps/acp/src/session_repository.rs" },
  { wave = 2, name = "snippet", spec = "22-snippet-command.md", module = "snippet",
    methods = "list, get, create, update, delete, changed", ref = "" },
  { wave = 2, name = "command", spec = "22-snippet-command.md", module = "command",
    methods = "list, get, create, update, delete, changed", ref = "" },
  { wave = 2, name = "plugin", spec = "23-plugin.md", module = "plugin",
    methods = "list, get, install, uninstall, enable, disable, configure, changed", ref = "" },
  { wave = 2, name = "quota-provider", spec = "24-quota-provider.md", module = "quota_provider",
    methods = "quota: get, list, reset; provider: list, get, set, test", ref = "" },
  { wave = 2, name = "agent", spec = "25-agent-profile.md", module = "agent_profile",
    methods = "list, get, create, update, delete, import, export, changed", ref = "apps/acp/src/agent_registry.rs" },
  { wave = 2, name = "diagnostics", spec = "26-diagnostics.md", module = "diagnostics",
    methods = "list, export, clear", ref = "" },
  { wave = 2, name = "project", spec = "27-project-config.md", module = "project",
    methods = "list, get, set, delete, import, export, changed", ref = "" },
  { wave = 2, name = "tunnel", spec = "28-tunnel.md", module = "tunnel",
    methods = "list, get, create, close, status, changed", ref = "" },
  { wave = 2, name = "multi-run", spec = "29-multi-run.md", module = "multi_run",
    methods = "create, cancel, status, list, get, changed, progress", ref = "anureo workflow system (luft)" },
  { wave = 2, name = "settings", spec = "30-settings.md", module = "settings",
    methods = "load, save, get, set, reset, changed", ref = "apps/acp/src/session_config_store.rs" },
  { wave = 2, name = "session-assist", spec = "31-session-assist.md", module = "session_assist",
    methods = "recap (notification only)", ref = "" },
  { wave = 2, name = "small-model", spec = "32-small-model.md", module = "small_model",
    methods = "generate, stream, summarize", ref = "apps/acp/src/high_freq_usage.rs" },
  { wave = 2, name = "auto-review", spec = "33-auto-review.md", module = "auto_review",
    methods = "start, status, cancel, result, configure", ref = "apps/acp/src/review_runner.rs" },
  { wave = 2, name = "preview", spec = "34-preview.md", module = "preview",
    methods = "render, list, get, close", ref = "" },
  { wave = 2, name = "terminal-ext", spec = "35-terminal.md", module = "terminal_ext",
    methods = "restart, force_kill, list, get",
    ref = "apps/acp/src/tools/terminal_executor.rs. Extends standard ACP terminal/*." },
  { wave = 2, name = "tts", spec = "19-tts-dictation.md", module = "tts",
    methods = "synthesize, summarize", ref = "WebSocket substream (08-cross-cutting §7)" },
  { wave = 2, name = "dictation", spec = "19-tts-dictation.md", module = "dictation",
    methods = "start, stop, stream", ref = "WebSocket substream (08-cross-cutting §7)" },
}

local ALL_MODULES = {}
for _, d in ipairs(DOMAINS) do table.insert(ALL_MODULES, d.module) end

-- ============================================================
-- Model config
-- ============================================================

local MODEL = "huoshan-coding-plan/deepseek-v4-flash-260425"

-- ============================================================
-- Schemas
-- ============================================================

local ARCH_SCHEMA = {
  type = "object",
  properties = {
    struct_name = { type = "string", description = "Rust struct name, e.g. FilesHandler" },
    rust_types = {
      type = "array",
      items = {
        type = "object",
        properties = {
          name = { type = "string" },
          kind = { type = "string", description = "struct / enum / type alias" },
          purpose = { type = "string" },
        },
        required = { "name", "kind", "purpose" },
      },
    },
    method_signatures = {
      type = "array",
      items = {
        type = "object",
        properties = {
          name = { type = "string" },
          params = { type = "string", description = "Key fields extracted from serde_json::Value" },
          result = { type = "string", description = "Return shape" },
          errors = { type = "array", items = { type = "string" }, description = "Expected error codes" },
        },
        required = { "name", "params", "result" },
      },
    },
    capability_json = { type = "string", description = "The JSON returned by capabilities()" },
    test_cases = { type = "array", items = { type = "string" } },
    framework_notes = { type = "string", description = "How it integrates with ExtensionHandler, pagination, boundary" },
    concerns = { type = "array", items = { type = "string" } },
  },
  required = { "struct_name", "rust_types", "method_signatures", "capability_json", "test_cases" },
}

local REVIEW_SCHEMA = {
  type = "object",
  properties = {
    approved = { type = "boolean" },
    spec_compliance_issues = { type = "array", items = { type = "string" } },
    framework_alignment_issues = { type = "array", items = { type = "string" } },
    edge_case_gaps = { type = "array", items = { type = "string" } },
    missing_error_handling = { type = "array", items = { type = "string" } },
    suggestions = { type = "array", items = { type = "string" } },
  },
  required = { "approved" },
}

local VERIFY_SCHEMA = {
  type = "object",
  properties = {
    compile_pass    = { type = "boolean" },
    clippy_pass     = { type = "boolean" },
    tests_pass      = { type = "boolean" },
    capability_ok   = { type = "boolean" },
    dispatch_ok     = { type = "boolean" },
    registration_ok = { type = "boolean" },
    relay_ok        = { type = "boolean" },
    domains_count   = { type = "integer" },
    issues          = { type = "array", items = { type = "string" } },
    fixes_applied   = { type = "array", items = { type = "string" } },
  },
  required = { "compile_pass", "tests_pass", "capability_ok", "dispatch_ok", "registration_ok" },
}

local CODE_REVIEW_SCHEMA = {
  type = "object",
  properties = {
    approved = { type = "boolean", description = "True if code is production-ready" },
    spec_violations = {
      type = "array",
      items = {
        type = "object",
        properties = {
          method = { type = "string" },
          issue = { type = "string" },
          severity = { type = "string", description = "critical / major / minor" },
        },
        required = { "method", "issue", "severity" },
      },
    },
    framework_violations = { type = "array", items = { type = "string" } },
    missing_methods = { type = "array", items = { type = "string" }, description = "Spec methods not implemented" },
    test_gaps = { type = "array", items = { type = "string" } },
    code_quality_issues = { type = "array", items = { type = "string" } },
    fixes_required = {
      type = "array",
      items = {
        type = "object",
        properties = {
          file = { type = "string" },
          issue = { type = "string" },
          fix = { type = "string", description = "Specific fix instruction" },
        },
        required = { "file", "issue", "fix" },
      },
    },
  },
  required = { "approved" },
}

local TEST_REVIEW_SCHEMA = {
  type = "object",
  properties = {
    approved = { type = "boolean", description = "True if tests are comprehensive and correct" },
    coverage_gaps = { type = "array", items = { type = "string" }, description = "Spec methods or branches without tests" },
    incorrect_tests = {
      type = "array",
      items = {
        type = "object",
        properties = {
          test_name = { type = "string" },
          issue = { type = "string" },
          fix = { type = "string" },
        },
        required = { "test_name", "issue", "fix" },
      },
    },
    missing_edge_cases = { type = "array", items = { type = "string" } },
    assertion_issues = { type = "array", items = { type = "string" }, description = "Weak or missing assertions" },
    fixes_required = {
      type = "array",
      items = {
        type = "object",
        properties = {
          file = { type = "string" },
          issue = { type = "string" },
          fix = { type = "string" },
        },
        required = { "file", "issue", "fix" },
      },
    },
  },
  required = { "approved" },
}

-- ============================================================
-- Prompt: Phase A (Quick fix)
-- ============================================================

local QUICK_FIX_PROMPT = [[
You are fixing two small issues in the anureo ACP backend (Rust).

## Task 1: Fix fork capability advertisement

In `apps/acp/src/agent.rs` around line 436, `SessionCapabilities` does not include `.fork(...)`. The handler is implemented (`agent.rs:731`) and registered (`stdio_loop.rs:236`), but capability is missing. Add it. Check agent-client-protocol crate 0.15.1 API.

## Task 2: Add diagnostic logging

In `apps/server/src/handlers/acp.rs`, add structured tracing logs at auth failure and connection lifecycle points.

## Verify: `cargo check -p anureo-acp && cargo check -p anureo-server`
Do NOT add comments. Follow existing code style.
]]

-- ============================================================
-- Prompt: Phase B (Framework)
-- ============================================================

local function make_module_list()
  local parts = {}
  for _, m in ipairs(ALL_MODULES) do table.insert(parts, "pub mod " .. m .. ";") end
  return table.concat(parts, "\n")
end

local FRAMEWORK_PROMPT = string.format(
[[You are building the `_anureo.dev/*` extension framework for the anureo ACP backend.

## Spec references (READ FIRST)
- `docs/acp-spec/08-cross-cutting-patterns.md` — §1 pagination, §2 auth, §3 progress, §4 capability, §8 error codes, §9 framework design
- `docs/acp-spec/00-overview.md`

## Key source files (READ THESE)
- `apps/acp/src/stdio_loop.rs` — dispatch loop. Line 112: `Agent.builder()`, line 523: `connect_with(transport, shutdown)`
- `apps/acp/src/runtime.rs` — `AcpRuntime` struct (ExtensionRegistry goes here)
- `apps/acp/src/agent.rs` — `initialize()` at line 419. `_meta` is empty; must add extension capabilities
- `apps/acp/src/lib.rs` — add `pub mod extensions;`
- `apps/server/src/acp_hub.rs` — where AcpRuntime is constructed

## Dispatch integration

Intercept messages whose `method` starts with `_anureo.dev/` BEFORE standard dispatch. Create a `Lines` wrapper that:
1. Reads each JSON-RPC line, parses `method`
2. If `_anureo.dev/*`: route to `ExtensionRegistry::dispatch()`
3. Otherwise: pass through to `Agent.builder()` dispatch

Read `agent_client_protocol` crate 0.15.1 source (`~/.cargo/registry/src/`) to understand `Lines` trait.

## Files to create

```
apps/acp/src/extensions/
├── mod.rs          ExtensionRegistry, ExtensionHandler trait, ExtensionContext, ExtensionError, dispatch()
├── capability.rs   CapabilityManager — snapshot + capability_changed
├── pagination.rs   PaginationParams, PaginatedResult, cursor encode/decode
├── progress.rs     ProgressReporter — anureo_progress via session/update
├── auth.rs         three-layer gate: capability → policy → confirm
└── boundary.rs     directory/worktree boundary validation
```

## Pre-declare ALL domain modules in mod.rs

```rust
%s
```

Create a stub file for each (content: `// Placeholder`). `cargo check` must pass.

## Core types

```rust
#[async_trait]
pub trait ExtensionHandler: Send + Sync {
    async fn handle(&self, method: &str, params: serde_json::Value, ctx: &ExtensionContext)
        -> Result<serde_json::Value, ExtensionError>;
    fn capabilities(&self) -> serde_json::Value;
}

pub struct ExtensionContext {
    pub session_id: Option<String>,
    pub principal: String,
    pub connection_id: String,
    pub working_directory: Option<PathBuf>,
}

pub struct ExtensionError { pub code: i32, pub message: String, pub data: Option<serde_json::Value> }
```

## Error codes (§8): -32601 method_not_found, -32602 invalid_params, -32001 capability_not_supported, -32002 forbidden, -32003 not_found, -32004 timeout, -32005 conflict, -32006 partial_failure, -32007 directory_boundary_violation

## DO NOT implement domain handlers — framework + stubs only.
## Verify: `cargo check -p anureo-acp` must pass.
]], make_module_list())

-- ============================================================
-- Prompt builders: 3-stage domain pipeline
-- ============================================================

-- Stage 1: Architecture design
local function make_arch_prompt(d)
  local ref_line = ""
  if d.ref and d.ref ~= "" then
    ref_line = "\n## Reference\n" .. d.ref
  end
  return string.format(
    [[You are the ARCHITECT for the `%s` extension domain. Design the implementation plan. Do NOT write code.

## Step 1: Read the spec

READ `docs/acp-spec/extensions/%s` COMPLETELY. Extract every JSON-RPC method, its params schema, result schema, error codes, capability key, and notification events.

## Step 2: Read the framework

Read `apps/acp/src/extensions/mod.rs` for the ExtensionHandler trait, ExtensionContext, ExtensionError.
Read `apps/acp/src/extensions/pagination.rs` for pagination helpers (if your domain has list methods).
Read `apps/acp/src/extensions/boundary.rs` for directory boundary checks (if your domain touches the filesystem).

## Step 3: Produce a design document

Design:
1. **Struct name**: The Rust struct implementing ExtensionHandler (e.g. `FilesHandler`)
2. **Rust types**: All structs/enums needed for params parsing and result serialization
3. **Method signatures**: For each method — params fields, result shape, expected error codes
4. **Capability JSON**: The exact JSON to return from `capabilities()`
5. **Test cases**: Specific test scenarios (params validation, success path, boundary, pagination)
6. **Framework integration**: How your handler uses pagination.rs, boundary.rs, etc.

Methods to cover:
%s

Return the design matching the schema. Be specific — the implementation agent will code directly from your design.

Do NOT write implementation code. Design only.]], d.name, d.spec, d.methods) .. ref_line
end

-- Stage 2: Adversarial review
local function make_review_prompt(d, design_json)
  return string.format(
    [[You are an ADVERSARIAL REVIEWER for the `%s` extension domain architecture. Your job is to find flaws.

## Spec

READ `docs/acp-spec/extensions/%s` COMPLETELY.

## Framework

Read `apps/acp/src/extensions/mod.rs` for ExtensionHandler trait and types.

## Architecture under review

```json
%s
```

## Review checklist

Check the design against ALL of the following. For each issue found, be specific (cite the exact method and field).

1. **Spec compliance**: Does the design cover EVERY method in the spec? Are param/result field names correct? Are error codes matching §8?

2. **Framework alignment**: Does the struct correctly implement ExtensionHandler? Does the dispatch routing match `_anureo.dev/%s/<method>`? Is the capability JSON shape correct?

3. **Edge cases**: Missing pagination on list methods? Missing boundary checks on filesystem operations? Missing session_id context? What about empty results vs fetch failures?

4. **Error handling**: Are ALL error paths from §8 covered? Is -32602 returned for bad params? Is -32001 for unsupported capability? Is -32007 for directory boundary violation?

5. **Reconnect resync**: Does the design account for `*_changed` notifications? Does each notification have a corresponding authoritative method?

6. **Security**: Are secrets/tokens excluded from responses? Are workspace boundaries enforced?

Be thorough and adversarial. If the design is solid, approve it. If not, list every issue.
]],
    d.name, d.spec, design_json, d.name
  )
end

-- Stage 3: Implementation
local function make_impl_prompt(d, design_json, review_json)
  local ref_line = ""
  if d.ref and d.ref ~= "" then
    ref_line = "\n## Reference\n" .. d.ref
  end
  return string.format(
    [[You are the IMPLEMENTER for the `%s` extension domain. Write code based on the verified architecture.

## Architecture design

```json
%s
```

## Adversarial review feedback

```json
%s
```

If the review raised issues, you MUST address them in your implementation. Do not ignore any issue.

## Framework

Read `apps/acp/src/extensions/mod.rs` for ExtensionHandler trait, ExtensionContext, ExtensionError.
Read `apps/acp/src/extensions/pagination.rs` if your design uses pagination.
Read `apps/acp/src/extensions/boundary.rs` if your design uses boundary checks.

## Spec

READ `docs/acp-spec/extensions/%s` for exact field names and JSON shapes.

## Implement

Fill in the EXISTING stub file `apps/acp/src/extensions/%s.rs` (or `%s/mod.rs` for multi-file). DO NOT modify mod.rs.

Implement ALL methods from the design. Each method must:
- Parse/validate params (return -32602 on bad input)
- Check boundary where applicable (use boundary.rs)
- Return serde_json::Value
- Use correct error codes
- Support pagination for list methods

Implement `capabilities()` to return the design's capability JSON.

## Do NOT write tests — a dedicated test engineer agent will handle that.
## Do NOT run cargo check. Do NOT modify mod.rs or register.rs.
## Do NOT add comments. Follow existing crate code style.]],
    d.name, design_json, review_json, d.spec, d.module, d.module
  ) .. ref_line
end

-- Stage 4: Code review
local function make_code_review_prompt(d, design_json)
  return string.format(
    [[You are a CODE REVIEWER for the `%s` extension domain. Review the actual implementation against the spec and architecture.

## Spec

READ `docs/acp-spec/extensions/%s` COMPLETELY.

## Framework

Read `apps/acp/src/extensions/mod.rs` for ExtensionHandler trait and types.

## Architecture design (reference)

```json
%s
```

## Code to review

Read the implementation file(s):
- `apps/acp/src/extensions/%s.rs` (or `apps/acp/src/extensions/%s/mod.rs` for multi-file domains)
- Also read any sub-module files under `%s/`

## Review checklist

Check the implementation against ALL of the following. For each issue, cite the exact file, line, and method.

1. **Spec compliance**: Is EVERY method from the spec implemented? Are param field names correct (camelCase vs snake_case serde)? Are result JSON shapes matching the spec? Are all `*_changed` notifications handled?

2. **Framework alignment**: Does the struct implement `ExtensionHandler` correctly? Is `handle()` dispatching to the right method based on the `method` parameter? Does `capabilities()` return the correct JSON shape?

3. **Error handling**: Is -32602 returned for missing/invalid params? Is -32001 for unsupported capability? Is -32007 for directory boundary violation? Are error messages informative?

4. **Pagination**: Do list methods accept `cursor` + `limit` params? Do they return `items` + `nextCursor` + `hasMore`? Is cursor opaque (base64 or token, not raw offset)?

5. **Boundary checks**: Do filesystem/git/worktree operations validate paths are within the working directory?

6. **Tests**: Are there tests for params validation, success path, boundary violation, pagination? Are the test cases from the architecture design covered?

7. **Code quality**: Proper error propagation (no unwrap/expect in production paths)? Correct use of async/await? No dead code? Imports clean?

8. **Security**: Are secrets/tokens/credentials excluded from responses? Are path traversal attempts blocked?

For each issue found, add a `fixes_required` entry with the specific file, issue description, and fix instruction.

If the code is production-ready, set `approved: true` with empty issue arrays.]],
    d.name, d.spec, design_json, d.module, d.module, d.module
  )
end

-- Stage 5: Fix based on code review
local function make_fix_prompt(d, review_json)
  return string.format(
    [[You are fixing issues found by code review in the `%s` extension domain.

## Code review feedback

```json
%s
```

## Your task

Read the implementation file(s):
- `apps/acp/src/extensions/%s.rs` (or `apps/acp/src/extensions/%s/mod.rs`)

Address EVERY `fixes_required` entry from the review. For each fix:
1. Locate the issue in the file
2. Apply the fix as described
3. If the fix instruction is unclear, use your judgment based on the spec

Also address any spec_violations, framework_violations, missing_methods, test_gaps, and code_quality_issues.

Read `docs/acp-spec/extensions/%s` if you need to verify spec details.
Read `apps/acp/src/extensions/mod.rs` if you need framework type details.

## Rules

- Do NOT modify mod.rs or register.rs
- Do NOT add comments
- Do NOT run cargo check
- Fix ALL issues listed in the review. Do not skip any.

If `approved: true` was returned by the review, there is nothing to do — return immediately.]],
    d.name, review_json, d.module, d.module, d.spec
  )
end

-- Stage 6: Test writing
local function make_test_prompt(d, design_json)
  return string.format(
    [[You are a TEST ENGINEER for the `%s` extension domain. Write comprehensive tests for the implementation.

## Spec

READ `docs/acp-spec/extensions/%s` COMPLETELY.

## Architecture design (reference)

```json
%s
```

## Implementation

Read `apps/acp/src/extensions/%s.rs` (or `%s/mod.rs`). Understand every method, branch, and error path.

## Framework

Read `apps/acp/src/extensions/mod.rs` for ExtensionHandler trait, ExtensionContext, ExtensionError.
Read `apps/acp/src/extensions/pagination.rs` if the domain has list methods.
Read `apps/acp/src/extensions/boundary.rs` if the domain touches the filesystem.

## Write tests

Add `#[cfg(test)]` module to the implementation file (or a companion test file). Cover:

1. **Happy path**: Each method with valid params returns correct result shape
2. **Params validation**: Missing required fields, wrong types, empty strings → -32602
3. **Error codes**: Unsupported capability → -32001, not found → -32003, forbidden → -32002, boundary violation → -32007
4. **Pagination**: cursor round-trip, empty list, hasMore true/false, limit clamping
5. **Boundary**: path traversal attempts blocked, paths outside workspace rejected
6. **Capability gate**: calling a method not in the capability snapshot fails
7. **Edge cases**: empty results, concurrent access, large inputs

Use the architecture design's `test_cases` list as minimum coverage.

## Rules

- Do NOT modify the production code (only add tests)
- Do NOT add comments
- Do NOT run cargo test
- Follow existing test patterns in the crate (check other test modules for style)]],
    d.name, d.spec, design_json, d.module, d.module
  )
end

-- Stage 7: Test review
local function make_test_review_prompt(d, design_json)
  return string.format(
    [[You are a TEST REVIEWER for the `%s` extension domain. Verify the test suite is comprehensive and correct.

## Spec

READ `docs/acp-spec/extensions/%s` COMPLETELY.

## Architecture design (reference)

```json
%s
```

## Implementation

Read `apps/acp/src/extensions/%s.rs` (or `%s/mod.rs`) to understand all methods, branches, and error paths.

## Tests to review

Read the `#[cfg(test)]` module in the implementation file (or companion test file).

## Review checklist

1. **Method coverage**: Does EVERY method from the spec have at least one test? List any untested methods.

2. **Branch coverage**: Are both success and error paths tested? Are all error codes exercised? Are edge cases (empty input, boundary, pagination) covered?

3. **Assertion quality**: Do tests actually assert the correct fields? Are there tests with weak assertions (e.g. `assert!(result.is_ok())` without checking the value)? Are negative tests asserting the right error code?

4. **Test correctness**: Will the tests actually compile? Are mock/stub setup correct? Are the expected values matching the spec?

5. **Missing scenarios**: What important scenarios are NOT tested? (concurrent access, large cursor, malicious input, capability revoked mid-operation)

6. **Spec alignment**: Do the test params match the exact field names from the spec (camelCase JSON)?

For each issue, add a `fixes_required` entry. If tests are comprehensive and correct, set `approved: true`.
]],
    d.name, d.spec, design_json, d.module, d.module
  )
end

-- Stage 8: Test fix
local function make_test_fix_prompt(d, review_json)
  return string.format(
    [[You are fixing test issues found by test review in the `%s` extension domain.

## Test review feedback

```json
%s
```

## Your task

Read the test code in `apps/acp/src/extensions/%s.rs` (or `%s/mod.rs`).

Address EVERY `fixes_required` entry. For each:
1. Locate the test or missing test
2. Add or fix the test as described
3. Ensure assertions are strong and correct

Also address any coverage_gaps, incorrect_tests, missing_edge_cases, and assertion_issues.

Read `docs/acp-spec/extensions/%s` for exact field names and error codes.

## Rules

- Do NOT modify production code
- Do NOT add comments
- Do NOT run cargo test
- Fix ALL issues. Do not skip any.

If `approved: true` was returned, return immediately.]],
    d.name, review_json, d.module, d.module, d.spec
  )
end

-- ============================================================
-- Other prompts
-- ============================================================

local COMPILE_GATE_PROMPT = [[
You are a compile gate. Compile-check the framework + Wave 1 domains and fix any issues.

```bash
cargo check -p anureo-acp
```

Fix ALL errors. You may modify framework files and Wave 1 domain files. Do NOT remove module declarations from mod.rs.

After fixing: `cargo check -p anureo-acp` must pass. Report what you fixed.
]]

local REGISTER_PROMPT = [[
Wire all extension domain handlers into ExtensionRegistry.

1. Read ALL files in `apps/acp/src/extensions/` to discover handler struct names.
2. Create `apps/acp/src/extensions/register.rs` with `register_default_extensions(&mut ExtensionRegistry)`.
3. Add `pub mod register;` to mod.rs.
4. Call it during AcpRuntime construction in `apps/acp/src/runtime.rs`.
5. Update `agent.rs` initialize() to pull capabilities from registry.

```cargo check -p anureo-acp && cargo check -p anureo-server```
]]

local RELAY_PROMPT = [[
Add `/acp` to the Relay WebSocket allowlist in `apps/server/src/`. Search for "relay", "allowlist", "tunnel".
Read `docs/acp-spec/07-transport.md`. `cargo check -p anureo-server`
]]

local VERIFY_PROMPT = [[
Final verification.

1. `cargo check -p anureo-acp && cargo check -p anureo-server && cargo clippy -p anureo-acp -- -D warnings`
2. `cargo test -p anureo-acp -- extensions && cargo test -p anureo-acp`
3. Verify initialize() has `.fork(...)` and `_meta["anureo.dev"]` lists all domains
4. Verify `_anureo.dev/*` routes to ExtensionRegistry in stdio_loop.rs
5. Verify register.rs cross-checks against files in extensions/
6. Fix any issues. Report findings.
]]

-- ============================================================
-- Helpers
-- ============================================================

local function filter_by_wave(domains, n)
  local out = {}
  for _, d in ipairs(domains) do
    if d.wave == n then table.insert(out, d) end
  end
  return out
end

-- Run one domain through: arch → arch-review → impl → [code-review → fix]×20 → [test → test-review → test-fix]×20
local function run_domain_pipeline(d)
  local result = { domain = d.name, arch_ok = false, review_ok = false, impl_ok = false,
                   code_review_ok = false, fix_ok = false,
                   test_ok = false, test_review_ok = false, test_fix_ok = false }

  -- Stage 1: Architecture
  local arch = agent({
    name = "arch-" .. d.name,
    description = "Design " .. d.name .. " architecture",
    prompt = make_arch_prompt(d),
    schema = ARCH_SCHEMA,
    model = MODEL,
  })
  if not arch.ok then
    result.status = "arch_failed: " .. arch.status
    return result
  end
  result.arch_ok = true
  local design_json = json.encode(arch.output)

  -- Stage 2: Adversarial architecture review
  local review = agent({
    name = "archreview-" .. d.name,
    description = "Adversarial review of " .. d.name .. " architecture",
    prompt = make_review_prompt(d, design_json),
    schema = REVIEW_SCHEMA,
    model = MODEL,
  })
  if not review.ok then
    result.status = "archreview_failed: " .. review.status
    return result
  end
  result.review_ok = true
  result.review_approved = review.output.approved
  local review_json = json.encode(review.output)

  -- Stage 3: Implementation
  local impl = agent({
    name = "impl-" .. d.name,
    description = "Implement " .. d.name .. " handler",
    prompt = make_impl_prompt(d, design_json, review_json),
    model = MODEL,
  })
  if not impl.ok then
    result.status = "impl_failed: " .. impl.status
    return result
  end
  result.impl_ok = true

  -- Stages 4+5: Code review → fix loop (max 20 iterations)
  local MAX_ROUNDS = 20
  local current_design = design_json

  for round = 1, MAX_ROUNDS do
    log(string.format("domain %s: code review round %d/%d", d.name, round, MAX_ROUNDS), "info")

    local cr = agent({
      name = "codereview-" .. d.name .. "-r" .. tostring(round),
      description = string.format("Code review %s round %d", d.name, round),
      prompt = make_code_review_prompt(d, current_design),
    schema = CODE_REVIEW_SCHEMA,
    model = MODEL,
  })
    if not cr.ok then
      result.status = "codereview_failed_r" .. tostring(round) .. ": " .. cr.status
      return result
    end
    result.code_review_ok = true
    result.code_review_approved = cr.output.approved
    result.code_review_rounds = round

    if cr.output.approved then
      result.fix_ok = true
      break
    end

    local cr_json = json.encode(cr.output)
    local remaining = MAX_ROUNDS - round
    if remaining == 0 then
      log(string.format("domain %s: code review not approved after %d rounds", d.name, MAX_ROUNDS), "warn")
      result.fix_ok = false
      result.status = "code_review_max_rounds"
      return result
    end

    log(string.format("domain %s: code round %d fixing (%d remaining)", d.name, round, remaining), "info")
    local fix = agent({
      name = "fix-" .. d.name .. "-r" .. tostring(round),
      description = string.format("Fix %s code round %d", d.name, round),
    prompt = make_fix_prompt(d, cr_json),
    model = MODEL,
  })
    if not fix.ok then
      result.status = "fix_failed_r" .. tostring(round) .. ": " .. fix.status
      return result
    end
  end

  -- Stages 6+7+8: Test → test-review → test-fix loop (max 5 iterations)
  log(string.format("domain %s: writing tests", d.name), "info")
  local test_agent = agent({
    name = "test-" .. d.name,
    description = "Write tests for " .. d.name,
    prompt = make_test_prompt(d, current_design),
    model = MODEL,
  })
  if not test_agent.ok then
    result.status = "test_write_failed: " .. test_agent.status
    return result
  end
  result.test_ok = true

  for round = 1, MAX_ROUNDS do
    log(string.format("domain %s: test review round %d/%d", d.name, round, MAX_ROUNDS), "info")

    local tr = agent({
      name = "testreview-" .. d.name .. "-r" .. tostring(round),
      description = string.format("Test review %s round %d", d.name, round),
      prompt = make_test_review_prompt(d, current_design),
    schema = TEST_REVIEW_SCHEMA,
    model = MODEL,
  })
    if not tr.ok then
      result.status = "testreview_failed_r" .. tostring(round) .. ": " .. tr.status
      return result
    end
    result.test_review_ok = true
    result.test_review_approved = tr.output.approved
    result.test_review_rounds = round

    if tr.output.approved then
      result.test_fix_ok = true
      result.status = "ok"
      return result
    end

    local tr_json = json.encode(tr.output)
    local remaining = MAX_ROUNDS - round
    if remaining == 0 then
      log(string.format("domain %s: test review not approved after %d rounds", d.name, MAX_ROUNDS), "warn")
      result.test_fix_ok = false
      result.status = "test_review_max_rounds"
      return result
    end

    log(string.format("domain %s: test round %d fixing (%d remaining)", d.name, round, remaining), "info")
    local tfix = agent({
      name = "testfix-" .. d.name .. "-r" .. tostring(round),
      description = string.format("Fix tests %s round %d", d.name, round),
    prompt = make_test_fix_prompt(d, tr_json),
    model = MODEL,
  })
    if not tfix.ok then
      result.status = "testfix_failed_r" .. tostring(round) .. ": " .. tfix.status
      return result
    end
  end

  result.status = "test_review_max_rounds"
  return result
end

-- Run a wave of domains through the 3-stage pipeline in parallel
local function run_wave(wave_num, wave_label)
  local items = filter_by_wave(DOMAINS, wave_num)
  log(string.format("Wave (%s): %d domains × 8 stages", wave_label, #items))

  local summary = {}
  local failures = 0
  for i, d in ipairs(items) do
    log(string.format("[%d/%d] starting domain: %s", i, #items, d.name))
    local r = run_domain_pipeline(d)
    table.insert(summary, {
      domain = d.name,
      arch_ok = r.arch_ok,
      review_ok = r.review_ok,
      review_approved = r.review_approved,
      impl_ok = r.impl_ok,
      code_review_ok = r.code_review_ok,
      code_review_approved = r.code_review_approved,
      code_review_rounds = r.code_review_rounds,
      fix_ok = r.fix_ok,
      test_ok = r.test_ok,
      test_review_ok = r.test_review_ok,
      test_review_approved = r.test_review_approved,
      test_review_rounds = r.test_review_rounds,
      test_fix_ok = r.test_fix_ok,
      status = r.status,
    })
    if r.status ~= "ok" then
      failures = failures + 1
      log(string.format("domain %s: %s", d.name, r.status), "warn")
    else
      log(string.format("[%d/%d] domain %s: OK", i, #items, d.name), "info")
    end
  end
  return summary, failures
end

-- ============================================================
-- Main
-- ============================================================

function main()
  local results = { waves = {} }

  -- Phase A (already completed — skip)
  phase("quick-fix")
  log("Phase A: SKIP (fork capability + logging already applied)")
  local fix = { ok = true, status = "skipped" }

  -- Phase B (already completed — skip)
  phase("framework")
  log("Phase B: SKIP (extension framework already built)")
  local framework = { ok = true, status = "skipped" }

  -- Phase C: Wave 1 (4 domains × arch→review→impl)
  phase("wave1-core")
  results.waves.wave1 = { label = "core" }
  local w1_ok, w1_domains, w1_failures = pcall(function()
    return run_wave(1, "files/git/worktree/mcp")
  end)
  if not w1_ok then
    log("Wave 1 Lua error: " .. tostring(w1_domains), "error")
    report({ error = "Wave 1 Lua error: " .. tostring(w1_domains) })
    return
  end
  results.waves.wave1.domains = w1_domains
  results.waves.wave1.failures = w1_failures

  -- Phase D: Compile gate
  phase("compile-gate")
  log("Phase D: compile gate")
  local gate = agent({
    name = "compile-gate", prompt = COMPILE_GATE_PROMPT, model = MODEL,
  })
  results.compile_gate = gate.ok
  if not gate.ok then log("compile gate failed — proceeding", "warn") end

  -- Phase E: Wave 2 (28 domains × arch→review→impl)
  phase("wave-rest")
  results.waves.wave2 = { label = "remaining" }
  results.waves.wave2.domains, results.waves.wave2.failures =
    run_wave(2, "28 remaining domains")

  -- Phase F: Registration + Relay (parallel)
  phase("register-relay")
  log("Phase F: registration + relay")
  local f_results = parallel({
    { task = "register" },
    { task = "relay" },
  }, function(item)
    if item.task == "register" then
      return { name = "register", prompt = REGISTER_PROMPT, model = MODEL }
    else
      return { name = "relay", prompt = RELAY_PROMPT, model = MODEL }
    end
  end)
  local reg_ok = f_results[1].ok
  local relay_ok = f_results[2].ok

  -- Phase G: Verification
  phase("verify")
  log("Phase G: final verification")
  local verify = agent({
    name = "verify", prompt = VERIFY_PROMPT, schema = VERIFY_SCHEMA, model = MODEL,
  })
  local verify_out = verify.ok and verify.output
    or { compile_pass = false, tests_pass = false, error = verify.status }

  -- Report
  phase("report")
  local total = 0
  local failed = 0
  for _, w in pairs(results.waves) do
    total = total + #w.domains
    failed = failed + w.failures
  end
  local ok_count = total - failed
  local partial = ok_count >= total - 3

  report({
    success = fix.ok and framework.ok and reg_ok and partial
      and (verify_out.compile_pass or false) and (verify_out.tests_pass or false)
      and (verify_out.capability_ok or false) and (verify_out.dispatch_ok or false)
      and (verify_out.registration_ok or false),
    partial_success = partial,
    phases = {
      fork_fix = fix.ok,
      framework = framework.ok,
      compile_gate = gate.ok,
      waves = {
        wave1 = { total = #results.waves.wave1.domains, failed = results.waves.wave1.failures },
        wave2 = { total = #results.waves.wave2.domains, failed = results.waves.wave2.failures },
      },
      registration = reg_ok,
      relay = relay_ok,
      verify = verify_out,
    },
    summary = string.format(
      "fork=%s fw=%s gate=%s domains(%d/%d) reg=%s relay=%s verify(c=%s t=%s caps=%s disp=%s)",
      tostring(fix.ok), tostring(framework.ok), tostring(gate.ok),
      ok_count, total, tostring(reg_ok), tostring(relay_ok),
      tostring(verify_out.compile_pass or false), tostring(verify_out.tests_pass or false),
      tostring(verify_out.capability_ok or false), tostring(verify_out.dispatch_ok or false)
    ),
    wave_details = results.waves,
  })
end
