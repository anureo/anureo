# anureo Workflow Guide

Workflows are suited for tasks that require multiple agents, parallel execution, or long-running repeated operations. They use Lua for task orchestration and save each execution's state, events, reports, and source snapshots as an **instance**.

> The workflow interface and instance model are still evolving. Do not use unverified automation for irreversible production operations; validate in an isolated project or worktree first.

## Creating a Workflow

Place `.lua` files in your project's `.anureo/workflows/`. The workflow DSL, agent parameters, and examples are in the built-in skill/reference under `agent/tool/tool-workflow/src/`; confirm discoverable definitions via anureo's workflow file list before writing.

Workflows should:

- Give each agent a clear prompt, name, and expected output.
- Accept task variables via `args` instead of hardcoding secrets in source.
- State in reports which conclusions are verified and which need human review.
- Keep concurrency within what the project and model provider can sustain.

## Starting and Observing

Workflows are started in the background by the agent's workflow tool, returning `instance_dir` and a `running` status. Recommended flow:

```text
workflow_start (script or workflow name)
→ record instance_dir
→ workflow_status(instance_dir)
→ wait and poll while status is running
```

Do not simultaneously read raw checkpoints, full events, and source. The terminal `workflow_status` provides a bounded summary: status, agent overview, phases, tokens, event statistics, and report preview.

## Diagnosing Failures

Troubleshoot in this order:

```text
workflow_list(status_filter="failed")
→ workflow_status(instance_dir)
→ workflow_events(instance_dir, types=["agent_done", "run_done"])
→ workflow_source(instance_dir) if needed
```

`workflow_events` supports offset, event limit, event type, and agent ID filters. Always start from the summary, then narrow the event scope as needed; large agent outputs or reports show only previews and controlled references.

## Cancellation and Resume

- `workflow_cancel` only accepts running instances owned by the current process; after a successful request, continue polling until the instance reaches `cancelled` or another terminal state.
- If the instance cannot be found, has already finished, or belongs to another process, cancellation returns a diagnostic status and does not affect other runs.
- When the runtime and checkpoint support it, you can create a new successor instance to resume from an interrupted one; it should record `resumed_from` and must not overwrite the source instance.
- Not all cancelled instances can be resumed. When resume is not possible, fix the input or script and restart; do not assume continuation from the interruption point.

## Instance Artifacts and Cleanup

Instances typically live in `.anureo/instances/<instance-dir>/`, containing a summary, checkpoint, events, source snapshot, and optional large reports/agent outputs. They are useful for auditing and troubleshooting but also consume disk space.

Before cleanup, confirm that the instance is no longer needed for recovery or diagnostics. During the compatibility period, old `.luft/runs/` records can be read and should be marked as `legacy`; new instances use `.anureo/`.
